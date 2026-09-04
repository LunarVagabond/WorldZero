using System;
using System.Collections.Concurrent;
using System.IO;
using System.Net.Security;
using System.Net.Sockets;
using System.Security.Cryptography.X509Certificates;
using System.Threading;
using System.Threading.Tasks;

namespace WorldZeroTestGrounds.Net;

// One TCP+TLS socket, one connection, for the whole session (PROMPT.md
// §2.1) — auth/realm/character/world/chat/plugin messages all
// multiplexed over it, distinguished only by each envelope's
// message_type. This class owns the raw socket + background read loop;
// NetworkClient.cs (the Godot autoload) owns decoding payloads into
// typed protobuf messages and dispatching them on the main thread.
public sealed class GameConnection : IDisposable
{
    // TLS choice (b) from PROMPT.md §2.1: disable certificate validation
    // entirely rather than pinning `server`'s self-signed
    // certs/self_signed.cert.der — acceptable for a disposable client
    // that only ever talks to localhost, documented in this project's
    // README.
    private static bool AcceptAnyServerCertificate(
        object sender,
        X509Certificate? certificate,
        X509Chain? chain,
        SslPolicyErrors sslPolicyErrors) => true;

    private TcpClient? _tcpClient;
    private SslStream? _sslStream;
    private Thread? _readThread;
    private volatile bool _running;
    private readonly object _writeLock = new();

    public readonly ConcurrentQueue<(ushort MessageType, byte[] Payload)> Incoming = new();

    // Set once the read loop exits, for whatever reason (clean close or
    // an I/O error) — drained on the main thread alongside Incoming so
    // the UI can react to disconnects without polling socket state
    // directly.
    public readonly ConcurrentQueue<string> DisconnectReasons = new();

    public bool IsConnected => _running && _tcpClient is { Connected: true };

    public async Task ConnectAsync(string host, int port)
    {
        _tcpClient = new TcpClient();
        await _tcpClient.ConnectAsync(host, port);

        _sslStream = new SslStream(_tcpClient.GetStream(), leaveInnerStreamOpen: false, AcceptAnyServerCertificate);
        // `server` generates a self-signed cert for "localhost"
        // (PROMPT.md §2.1) regardless of what host we actually dialed.
        await _sslStream.AuthenticateAsClientAsync("localhost");

        _running = true;
        _readThread = new Thread(ReadLoop) { IsBackground = true, Name = "GameConnection-Read" };
        _readThread.Start();
    }

    public void Send(ushort messageType, byte[] payload)
    {
        if (_sslStream is null)
        {
            throw new InvalidOperationException("Send called before ConnectAsync completed");
        }
        lock (_writeLock)
        {
            Envelope.Write(_sslStream, messageType, payload);
        }
    }

    private void ReadLoop()
    {
        try
        {
            while (_running)
            {
                var frame = Envelope.ReadOne(_sslStream!);
                if (frame is null)
                {
                    DisconnectReasons.Enqueue("connection closed by server");
                    break;
                }
                Incoming.Enqueue(frame.Value);
            }
        }
        catch (Exception ex)
        {
            DisconnectReasons.Enqueue($"connection error: {ex.Message}");
        }
        finally
        {
            _running = false;
        }
    }

    public void Disconnect()
    {
        _running = false;
        try { _sslStream?.Close(); } catch { /* already gone */ }
        try { _tcpClient?.Close(); } catch { /* already gone */ }
    }

    public void Dispose() => Disconnect();
}
