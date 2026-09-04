namespace WorldZeroTestGrounds.State;

// One entry in the local roster GameState keeps for the current zone+layer
// (PROMPT.md §15) — seeded from Joined/ZoneChanged.roster, updated by
// EntitySpawned/EntityDespawned/Moved. Position here is always the last
// confirmed server value; interpolation state for rendering lives on the
// visual node itself (Movement/EntityInterpolator.cs), not here.
public sealed class RosterEntry
{
    public required string EntityId { get; init; }
    public required string EntityType { get; set; }
    public double X { get; set; }
    public double Y { get; set; }
    public double PrevX { get; set; }
    public double PrevY { get; set; }
    public ulong LastTick { get; set; }
    public double LastUpdateUnixMs { get; set; }
}
