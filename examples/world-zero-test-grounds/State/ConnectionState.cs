namespace WorldZeroTestGrounds.State;

// PROMPT.md §15: "entirely your own state machine, nothing server-driven
// beyond message arrival/socket closure."
public enum ConnectionState
{
    Disconnected,
    Connecting,
    Connected,
    Authenticating,
    RealmSelect,
    CharacterSelect,
    InWorld,
}
