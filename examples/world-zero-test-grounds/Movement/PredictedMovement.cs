using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Movement;

// Client-side prediction/reconciliation for the connection's OWN entity
// (PROMPT.md §6.4) — the correlation mechanism `Move.seq` /
// `Moved.seq`/`Rejected.seq` (#196) makes real now. Operates on
// GameState's PredictedX/Y, AuthoritativeX/Y, and PendingMoves buffer;
// Scenes/World's player controller calls these from the matching
// NetworkClient events and owns turning WASD input into the next
// predicted target itself.
public static class PredictedMovement
{
    // Call before sending a new Move{x,y,seq} — predicts immediately
    // (the whole point of client-side prediction: you have the input,
    // you get to guess) and remembers it for later reconciliation.
    public static void RecordPredictedMove(uint seq, double x, double y)
    {
        var gs = GameState.Instance;
        gs.PredictedX = x;
        gs.PredictedY = y;
        gs.PendingMoves.Add((seq, x, y));
    }

    // Call when a Moved for this connection's own entity_id arrives
    // (seq != 0, per §6.3's "seq is 0 for a move that didn't originate
    // from a real client Move" note).
    public static void ReconcileConfirmed(uint confirmedSeq, double confirmedX, double confirmedY, ulong tick)
    {
        var gs = GameState.Instance;
        gs.AuthoritativeX = confirmedX;
        gs.AuthoritativeY = confirmedY;
        gs.LastTick = tick;
        gs.PendingMoves.RemoveAll(p => p.Seq <= confirmedSeq);
        if (gs.PendingMoves.Count == 0)
        {
            // Fully caught up — snap predicted to authoritative so
            // small floating-point/timing drift never accumulates.
            gs.PredictedX = confirmedX;
            gs.PredictedY = confirmedY;
        }
    }

    // Call when a Rejected for this connection's own move arrives.
    // Discards the rejected step and everything predicted after it
    // (they were all predicted forward from a step that turned out
    // invalid), then re-predicts forward from the last confirmed
    // position — this test client does that by simply snapping back
    // rather than replaying the discarded inputs, which is enough for a
    // manual test tool (a real game would replay them against the new
    // authoritative baseline).
    public static void ReconcileRejected(uint rejectedSeq, ulong tick)
    {
        var gs = GameState.Instance;
        gs.LastTick = tick;
        gs.PendingMoves.RemoveAll(p => p.Seq >= rejectedSeq);
        gs.PredictedX = gs.AuthoritativeX;
        gs.PredictedY = gs.AuthoritativeY;
    }

    // Call on Joined/ZoneChanged for this connection's own entity — a
    // hard snap, never interpolated or reconciled (§6.4's own note: a
    // teleport/zone change is a different message entirely from Moved).
    public static void HardSet(double x, double y, ulong tick)
    {
        var gs = GameState.Instance;
        gs.AuthoritativeX = x;
        gs.AuthoritativeY = y;
        gs.PredictedX = x;
        gs.PredictedY = y;
        gs.LastTick = tick;
        gs.PendingMoves.Clear();
    }
}
