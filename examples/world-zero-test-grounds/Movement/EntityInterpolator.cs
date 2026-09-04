namespace WorldZeroTestGrounds.Movement;

// Interpolates a non-owned entity's visual position between the last two
// `Moved` (or `EntitySpawned`) snapshots it received — PROMPT.md §6.4:
// "you have no input signal for other entities — just periodic Moved
// snapshots — so purely interpolate other entities... don't try to
// predict them." No extrapolation past the newest sample; once the
// render clock catches up to it, position just holds there until the
// next snapshot arrives (a 20Hz tick means that's rarely more than
// ~50ms of held position under normal conditions).
public sealed class EntityInterpolator
{
    private double _prevX, _prevY, _prevAtMs;
    private double _curX, _curY, _curAtMs;
    private bool _hasSample;

    public void HardSet(double x, double y, double nowMs)
    {
        _prevX = _curX = x;
        _prevY = _curY = y;
        _prevAtMs = _curAtMs = nowMs;
        _hasSample = true;
    }

    public void PushSample(double x, double y, double nowMs)
    {
        if (!_hasSample)
        {
            HardSet(x, y, nowMs);
            return;
        }
        _prevX = _curX; _prevY = _curY; _prevAtMs = _curAtMs;
        _curX = x; _curY = y; _curAtMs = nowMs;
    }

    public (double X, double Y) GetInterpolated(double nowMs)
    {
        if (!_hasSample)
        {
            return (0, 0);
        }
        double span = _curAtMs - _prevAtMs;
        if (span <= 0.0)
        {
            return (_curX, _curY);
        }
        double t = (nowMs - _prevAtMs) / span;
        if (t < 0.0) t = 0.0;
        if (t > 1.0) t = 1.0;
        return (_prevX + (_curX - _prevX) * t, _prevY + (_curY - _prevY) * t);
    }
}
