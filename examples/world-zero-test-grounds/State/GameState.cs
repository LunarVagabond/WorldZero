using System;
using System.Collections.Generic;
using Godot;

namespace WorldZeroTestGrounds.State;

// The autoload singleton holding everything PROMPT.md §15 says a client
// SDK should track — a plain state bag updated as messages arrive, no
// formal SDK abstraction. Scenes read this directly rather than each
// keeping their own copy, since almost everything here (roster, party,
// guild, stats) needs to be visible from more than one panel at once.
public partial class GameState : Node
{
    public static GameState Instance { get; private set; } = null!;

    private const int EventLogCapacity = 2000;

    public override void _EnterTree()
    {
        Instance = this;
    }

    // --- Identity (§15) ---
    public string? AccountId;
    public string? Username;
    public string? SessionToken;
    public string? RealmId;
    public string? CharacterId;
    public string? EntityId;
    public string? ZoneId;

    // --- Connection state machine ---
    public ConnectionState ConnectionState { get; private set; } = ConnectionState.Disconnected;
    public event Action<ConnectionState>? ConnectionStateChanged;

    public void SetConnectionState(ConnectionState state)
    {
        ConnectionState = state;
        ConnectionStateChanged?.Invoke(state);
    }

    // --- Position: authoritative vs predicted (§6.4/§16) ---
    public double AuthoritativeX;
    public double AuthoritativeY;
    public double PredictedX;
    public double PredictedY;
    public ulong LastTick;

    // In-flight Move requests not yet confirmed/rejected — (seq, predicted x/y at send time).
    public readonly List<(uint Seq, double X, double Y)> PendingMoves = new();

    // --- Local roster: entity_id -> {type, last known x/y, last update} (§15) ---
    public readonly Dictionary<string, RosterEntry> Roster = new();

    // --- Chat: joined channel name -> channel_id (§15) ---
    public readonly Dictionary<string, string> ChatChannels = new();

    // --- Party (§11/§15): every OTHER member's entity id ---
    public readonly List<string> PartyMembers = new();

    // --- Guild (§12/§15) ---
    public string? GuildId;
    public string? GuildName;
    public string? GuildMotd;
    public string? GuildTag;
    public readonly List<(string EntityId, string RankKey)> GuildMembers = new();
    public string? MyGuildRankKey; // derived: the GuildMembers entry whose EntityId == this connection's own EntityId.

    // --- Account roles (docs/specs/Auth_Spec.md's real, backend-enforced
    // account_roles system) — learned via evil-cube-plugin's ad-hoc
    // `roles:` PluginMessage convention on zone join, since core has no
    // wire message telling a client its own roles (caller-role is
    // plugin-facing only). Every admin action still independently
    // re-checks caller-role server-side; this is only used to decide
    // whether to show the admin panel at all. Fails closed: IsAdmin is
    // false until a real announcement says otherwise. ---
    public readonly List<string> Roles = new();
    public bool IsAdmin => Roles.Contains("admin");
    public event Action? RolesChanged;

    public void SetRoles(IEnumerable<string> roles)
    {
        Roles.Clear();
        Roles.AddRange(roles);
        RolesChanged?.Invoke();
    }

    // --- Current target (§7.3): purely local UI state ---
    public string? CurrentTargetEntityId;

    // --- Movement lock while typing: `Input.IsKeyPressed` in
    // WorldController bypasses normal Godot focus/input-consumption, so
    // without this, typing in any HUD text field (chat, party/guild
    // target, crafting, admin) also drives WASD movement in the 3D view.
    // Set true on FocusEntered, false on FocusExited — see
    // UiHelpers.LockMovementWhileFocused, wired onto every LineEdit in
    // the in-world HUD tabs. ---
    public bool TextInputActive;

    // --- Own character stats/items/currency, from StatChanged/ItemChanged/CurrencyChanged (§4/§13/§15) ---
    public readonly Dictionary<string, long> OwnStats = new();
    public readonly Dictionary<string, long> OwnItems = new();
    public readonly Dictionary<string, long> OwnCurrency = new();

    // --- Evil Cube HP, parsed from the ad-hoc `cube:` PluginMessage convention (§7.1) — kept
    // separate from OwnStats/etc. since it's NPC state, not a structured push. Keyed by entity_id. ---
    public readonly Dictionary<string, (long Current, long Max, bool Dead)> CubeHp = new();

    // --- Ping (§6.3/§16) ---
    public long LastPingSentAtMs;
    public double LastRttMs = -1;
    public double LastClockSkewMs;

    // --- Event/message log console (§16 — required, not to be minimized) ---
    public readonly LinkedList<EventLogEntry> EventLog = new();
    public event Action<EventLogEntry>? EventLogged;

    public void LogEvent(string category, string summary, bool quiet = false)
    {
        var entry = new EventLogEntry(Time.GetUnixTimeFromSystem() * 1000.0, category, summary, quiet);
        EventLog.AddLast(entry);
        while (EventLog.Count > EventLogCapacity)
        {
            EventLog.RemoveFirst();
        }
        EventLogged?.Invoke(entry);
    }

    public void ResetForNewCharacter()
    {
        Roster.Clear();
        PartyMembers.Clear();
        GuildId = null;
        GuildName = null;
        GuildMotd = null;
        GuildTag = null;
        GuildMembers.Clear();
        MyGuildRankKey = null;
        CurrentTargetEntityId = null;
        OwnStats.Clear();
        OwnItems.Clear();
        OwnCurrency.Clear();
        CubeHp.Clear();
        PendingMoves.Clear();
    }
}
