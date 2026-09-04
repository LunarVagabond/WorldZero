namespace WorldZeroTestGrounds.Net;

// PROMPT.md §2.3's message_type catalog (docs/specs/Networking_Spec.md
// in world_zero). >= 1000 is plugin-declared/opaque; this test grounds
// never sends or expects one, so it isn't listed here.
public static class MessageType
{
    public const ushort Auth = 1;
    public const ushort Realm = 2;
    public const ushort Character = 3;
    public const ushort Chat = 100;
    public const ushort Session = 200;
}
