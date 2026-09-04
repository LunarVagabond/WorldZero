namespace WorldZeroTestGrounds.State;

// One line in §16's required event/message log console — every decoded
// envelope, its category (roughly, which protocol/message_type it came
// from), and a one-line summary.
public readonly struct EventLogEntry
{
    public EventLogEntry(double timeUnixMs, string category, string summary, bool isQuiet)
    {
        TimeUnixMs = timeUnixMs;
        Category = category;
        Summary = summary;
        IsQuiet = isQuiet;
    }

    public double TimeUnixMs { get; }
    public string Category { get; }
    public string Summary { get; }

    // Movement traffic (Move/Moved) fires at up to 20Hz and would drown
    // out everything else — still logged (PROMPT.md §16 says not to skip
    // or minimize the console), just flagged so the UI can offer a
    // "hide movement spam" filter without ever dropping the messages
    // from the underlying log.
    public bool IsQuiet { get; }
}
