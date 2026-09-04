using System;
using System.Buffers.Binary;
using System.IO;

namespace WorldZeroTestGrounds.Net;

// Wire framing for every message on the connection (PROMPT.md §2.2,
// mirrors `world_zero/crates/gateway/src/envelope.rs`'s
// `EnvelopeCodec`/`tokio_util::codec::LengthDelimitedCodec`, default
// config):
//
//   [ 4-byte big-endian u32: length of everything that follows ]
//   [ 2-byte big-endian u16: message_type ]
//   [ N bytes: protobuf-encoded payload ]
//
// The length field covers only the type + payload, not itself — verified
// against `envelope.rs`'s own round-trip tests
// (`Envelope::new(3, b"payload")` encodes a 4-byte length of 9, i.e.
// 2 + 7, then the 2-byte type, then the 7 payload bytes).
public static class Envelope
{
    public static void Write(Stream stream, ushort messageType, ReadOnlySpan<byte> payload)
    {
        Span<byte> header = stackalloc byte[6];
        BinaryPrimitives.WriteUInt32BigEndian(header[..4], (uint)(2 + payload.Length));
        BinaryPrimitives.WriteUInt16BigEndian(header.Slice(4, 2), messageType);
        stream.Write(header);
        stream.Write(payload);
        stream.Flush();
    }

    // Blocks until a full frame is available, or returns null on a clean
    // EOF that lands exactly on a frame boundary (nothing read yet).
    public static (ushort MessageType, byte[] Payload)? ReadOne(Stream stream)
    {
        Span<byte> lenBuf = stackalloc byte[4];
        if (!ReadExact(stream, lenBuf))
        {
            return null;
        }

        uint frameLength = BinaryPrimitives.ReadUInt32BigEndian(lenBuf);
        if (frameLength < 2)
        {
            throw new IOException($"envelope too short: {frameLength} bytes, need at least 2");
        }

        byte[] frame = new byte[frameLength];
        if (!ReadExact(stream, frame))
        {
            throw new IOException("connection closed mid-frame");
        }

        ushort messageType = BinaryPrimitives.ReadUInt16BigEndian(frame.AsSpan(0, 2));
        byte[] payload = new byte[frame.Length - 2];
        Array.Copy(frame, 2, payload, 0, payload.Length);
        return (messageType, payload);
    }

    private static bool ReadExact(Stream stream, Span<byte> buffer)
    {
        int total = 0;
        while (total < buffer.Length)
        {
            int read = stream.Read(buffer[total..]);
            if (read == 0)
            {
                if (total == 0)
                {
                    return false;
                }
                throw new IOException("connection closed mid-frame");
            }
            total += read;
        }
        return true;
    }
}
