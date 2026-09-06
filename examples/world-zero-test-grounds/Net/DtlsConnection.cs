using System;
using System.Collections.Concurrent;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;
using System.Threading;
using System.Threading.Tasks;
using Org.BouncyCastle.Security;
using Org.BouncyCastle.Tls;
using Org.BouncyCastle.Tls.Crypto.Impl.BC;

namespace WorldZeroTestGrounds.Net;

// The optional UDP/DTLS movement channel (#295, PROMPT.md §2.1) — a
// second, independent connection alongside `GameConnection`'s TCP+TLS
// one. .NET has no built-in DTLS (`SslStream` is TCP-only), so this uses
// BouncyCastle's `Org.BouncyCastle.Tls` DTLS client directly against a
// real `System.Net.Sockets.UdpClient`, the same "drive the handshake
// yourself against a real socket" shape `gateway::udp` uses on the Rust
// side. Same shape as `GameConnection`: owns the socket + a background
// read loop, exposes decoded `(MessageType, Payload)` frames on a queue
// for `NetworkClient` to dispatch on the main thread.
public sealed class DtlsConnection : IDisposable
{
    // Matches GameConnection's own documented, already-established
    // project convention (`GameConnection.cs`'s
    // `AcceptAnyServerCertificate`, PROMPT.md §2.1 choice (b)): accept the
    // server's certificate unconditionally rather than pinning it —
    // acceptable for a disposable client that only ever talks to
    // localhost.
    private sealed class AcceptAnyServerAuthentication : TlsAuthentication
    {
        public void NotifyServerCertificate(TlsServerCertificate serverCertificate)
        {
            // Deliberately a no-op — see the class doc comment.
        }

        public TlsCredentials? GetClientCredentials(CertificateRequest certificateRequest) => null;
    }

    // A `TlsClient` requesting DTLS 1.2 specifically (`DtlsClientProtocol`
    // only ever drives a DTLS handshake, but `AbstractTlsClient`'s default
    // supported-versions list is shared with the TLS side) and accepting
    // any server certificate via `AcceptAnyServerAuthentication` above.
    private sealed class InsecureDtlsClient : DefaultTlsClient
    {
        public InsecureDtlsClient(Org.BouncyCastle.Tls.Crypto.TlsCrypto crypto) : base(crypto) { }

        public override TlsAuthentication GetAuthentication() => new AcceptAnyServerAuthentication();

        protected override ProtocolVersion[] GetSupportedVersions() => ProtocolVersion.DTLSv12.Only();
    }

    // `Org.BouncyCastle.Tls.DatagramTransport` driven against a real,
    // already-connected `UdpClient` — the same "BouncyCastle owns no
    // socket, the caller drives it" shape `gateway::udp`'s sans-IO
    // `rtc-dtls` has on the Rust side, just with BouncyCastle's own
    // blocking-with-timeout `Receive` contract instead of Rust's
    // channels/`tokio::select!`.
    private sealed class UdpDatagramTransport : DatagramTransport
    {
        // Comfortably under a typical Ethernet MTU (1500) once IP/UDP/DTLS
        // record overhead is accounted for — matches `gateway::udp`'s own
        // `recv_buf` sizing headroom on the Rust side.
        private const int Mtu = 1400;

        private readonly Socket _socket;

        public UdpDatagramTransport(Socket socket) => _socket = socket;

        public int GetReceiveLimit() => Mtu;

        public int GetSendLimit() => Mtu;

        public int Receive(byte[] buf, int off, int len, int waitMillis)
        {
            _socket.ReceiveTimeout = waitMillis;
            try
            {
                return _socket.Receive(buf, off, len, SocketFlags.None);
            }
            catch (SocketException ex) when (ex.SocketErrorCode == SocketError.TimedOut)
            {
                // The `DatagramTransport` contract: -1 means "nothing
                // arrived within waitMillis," not an error — the DTLS
                // handshake state machine uses this to drive its own
                // retransmission timers.
                return -1;
            }
        }

        public int Receive(Span<byte> buffer, int waitMillis)
        {
            _socket.ReceiveTimeout = waitMillis;
            try
            {
                return _socket.Receive(buffer, SocketFlags.None);
            }
            catch (SocketException ex) when (ex.SocketErrorCode == SocketError.TimedOut)
            {
                return -1;
            }
        }

        public void Send(byte[] buf, int off, int len) => _socket.Send(buf, off, len, SocketFlags.None);

        public void Send(ReadOnlySpan<byte> buffer) => _socket.Send(buffer, SocketFlags.None);

        public void Close() => _socket.Close();
    }

    private UdpClient? _udpClient;
    private DtlsTransport? _dtlsTransport;
    private Thread? _readThread;
    private volatile bool _running;
    private readonly object _writeLock = new();

    public readonly ConcurrentQueue<(ushort MessageType, byte[] Payload)> Incoming = new();
    public readonly ConcurrentQueue<string> DisconnectReasons = new();

    public bool IsConnected => _running && _dtlsTransport is not null;

    public async Task ConnectAsync(string host, int port)
    {
        _udpClient = new UdpClient();
        _udpClient.Connect(host, port);

        // `DtlsClientProtocol.Connect` blocks for the whole handshake
        // (including its own retransmission waits against
        // `UdpDatagramTransport.Receive`'s timeouts) — run it off the
        // calling async context's thread, same reasoning as
        // `GameConnection`'s background read thread.
        _dtlsTransport = await Task.Run(() =>
        {
            var crypto = new BcTlsCrypto(new SecureRandom());
            var client = new InsecureDtlsClient(crypto);
            var protocol = new DtlsClientProtocol();
            var transport = new UdpDatagramTransport(_udpClient.Client);
            return protocol.Connect(client, transport);
        });

        _running = true;
        _readThread = new Thread(ReadLoop) { IsBackground = true, Name = "DtlsConnection-Read" };
        _readThread.Start();
    }

    public void Send(ushort messageType, byte[] payload)
    {
        if (_dtlsTransport is null)
        {
            throw new InvalidOperationException("Send called before ConnectAsync completed");
        }
        byte[] datagram = Envelope.WriteDatagram(messageType, payload);
        lock (_writeLock)
        {
            _dtlsTransport.Send(datagram, 0, datagram.Length);
        }
    }

    private void ReadLoop()
    {
        // `Mtu` from `UdpDatagramTransport`, kept in sync by using the
        // same constant via `GetReceiveLimit()` rather than a second
        // hardcoded literal.
        byte[] buf = new byte[_dtlsTransport!.GetReceiveLimit()];
        try
        {
            while (_running)
            {
                // A generous per-call wait — this is the post-handshake
                // steady-state read, not the handshake's own
                // retransmission timing, so there's no tight deadline to
                // hit; it just needs to periodically re-check `_running`.
                int n = _dtlsTransport.Receive(buf, 0, buf.Length, 1000);
                if (n < 0)
                {
                    continue; // timeout, not a disconnect — keep waiting
                }
                var (messageType, payload) = Envelope.ReadDatagram(buf.AsSpan(0, n));
                Incoming.Enqueue((messageType, payload));
            }
        }
        catch (Exception ex)
        {
            DisconnectReasons.Enqueue($"UDP connection error: {ex.Message}");
        }
        finally
        {
            _running = false;
        }
    }

    public void Disconnect()
    {
        _running = false;
        try { _dtlsTransport?.Close(); } catch { /* already gone */ }
        try { _udpClient?.Close(); } catch { /* already gone */ }
    }

    public void Dispose() => Disconnect();
}
