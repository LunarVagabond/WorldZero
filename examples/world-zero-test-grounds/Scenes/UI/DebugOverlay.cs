using System.Linq;
using Godot;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes.UI;

// The §16 dashboard — "the single most useful thing for the human
// running this tool to actually watch World Zero's real lifecycle
// during manual testing — don't skip it or make it minimal."
public partial class DebugOverlay : Control
{
    private Label _idsLabel = null!;
    private Label _posLabel = null!;
    private Label _pingLabel = null!;
    private Label _rosterLabel = null!;
    private Label _partyGuildLabel = null!;
    private Label _statsLabel = null!;
    private RichTextLabel _eventLog = null!;
    private CheckBox _showQuietCheck = null!;

    public override void _Ready()
    {
        SetAnchorsPreset(LayoutPreset.FullRect);
        var box = UiHelpers.CreateScrollableColumn(this);

        _idsLabel = NewLabel(box);
        _posLabel = NewLabel(box);
        _pingLabel = NewLabel(box);
        _rosterLabel = NewLabel(box);
        _partyGuildLabel = NewLabel(box);
        _statsLabel = NewLabel(box);

        box.AddChild(new HSeparator());
        var logHeader = new HBoxContainer();
        box.AddChild(logHeader);
        logHeader.AddChild(new Label { Text = "Event log" });
        _showQuietCheck = new CheckBox { Text = "show movement spam (Move/Moved)" };
        _showQuietCheck.Toggled += _ => RebuildLog();
        logHeader.AddChild(_showQuietCheck);

        _eventLog = new RichTextLabel
        {
            ScrollFollowing = true,
            SizeFlagsVertical = SizeFlags.ExpandFill,
            CustomMinimumSize = new Vector2(0, 260),
        };
        box.AddChild(_eventLog);

        GameState.Instance.EventLogged += OnEventLogged;
        RebuildLog();
    }

    private static Label NewLabel(Control parent) => UiHelpers.AddWrappingLabel(parent);

    private void RebuildLog()
    {
        _eventLog.Clear();
        foreach (var e in GameState.Instance.EventLog)
        {
            if (e.IsQuiet && !_showQuietCheck.ButtonPressed)
            {
                continue;
            }
            AppendLine(e);
        }
    }

    private void OnEventLogged(EventLogEntry e)
    {
        if (e.IsQuiet && !_showQuietCheck.ButtonPressed)
        {
            return;
        }
        AppendLine(e);
    }

    private void AppendLine(EventLogEntry e)
    {
        _eventLog.AppendText($"[{e.Category}] {e.Summary}\n");
    }

    public override void _Process(double delta)
    {
        var gs = GameState.Instance;
        _idsLabel.Text = $"state={gs.ConnectionState}  account={Short(gs.AccountId)}  realm={Short(gs.RealmId)}  character={Short(gs.CharacterId)}  entity={Short(gs.EntityId)}  zone={gs.ZoneId ?? "-"}  layer=not exposed by protocol (§8)";

        _posLabel.Text = $"authoritative=({gs.AuthoritativeX:F2},{gs.AuthoritativeY:F2})  predicted=({gs.PredictedX:F2},{gs.PredictedY:F2})  drift={(new Vector2((float)(gs.PredictedX - gs.AuthoritativeX), (float)(gs.PredictedY - gs.AuthoritativeY))).Length():F2}m  pending_moves={gs.PendingMoves.Count}  tick={gs.LastTick}";

        string ping = gs.LastRttMs < 0 ? "ping: (waiting for first Pong)" : $"ping: {gs.LastRttMs:F0}ms rtt  clock_skew={gs.LastClockSkewMs:F0}ms";
        _pingLabel.Text = ping;

        int playerCount = gs.Roster.Values.Count(r => r.EntityType != "npc");
        int npcCount = gs.Roster.Count - playerCount;
        _rosterLabel.Text = $"visible in zone: {playerCount} player(s), {npcCount} npc(s)  target={Short(gs.CurrentTargetEntityId)}";

        string party = gs.PartyMembers.Count == 0 ? "no party" : $"party members: {string.Join(", ", gs.PartyMembers.Select(Short))}";
        string guild = gs.GuildId is null ? "no guild" : $"guild: {gs.GuildName} [{gs.GuildTag}] motd=\"{gs.GuildMotd}\" rank={gs.MyGuildRankKey} members={gs.GuildMembers.Count}";
        _partyGuildLabel.Text = $"{party}   |   {guild}";

        string cube = gs.CubeHp.Count == 0
            ? "evil cube: (no hit yet)"
            : string.Join("; ", gs.CubeHp.Select(kv => $"cube[{Short(kv.Key)}]={(kv.Value.Dead ? "DEAD" : $"{kv.Value.Current}/{kv.Value.Max}")}"));
        string stats = gs.OwnStats.Count == 0 ? "stats: (none yet)" : "stats: " + string.Join(", ", gs.OwnStats.Select(kv => $"{kv.Key}={kv.Value}"));
        string items = gs.OwnItems.Count == 0 ? "items: (none yet)" : "items: " + string.Join(", ", gs.OwnItems.Select(kv => $"{kv.Key}={kv.Value}"));
        string currency = gs.OwnCurrency.Count == 0 ? "currency: (none yet)" : "currency: " + string.Join(", ", gs.OwnCurrency.Select(kv => $"{kv.Key}={kv.Value}"));
        _statsLabel.Text = $"{stats}\n{items}\n{currency}\n{cube}";
    }

    private static string Short(string? id) => id is null ? "-" : id.Length > 8 ? id[..8] : id;
}
