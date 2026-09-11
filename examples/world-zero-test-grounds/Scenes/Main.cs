using Godot;
using WorldZeroTestGrounds.Net;
using WorldZeroTestGrounds.Scenes.UI;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes;

// The root scene — orchestrates the mandatory three-stage handshake
// (auth -> realm select -> character select -> automatic world-join,
// PROMPT.md §2.5) and swaps which panel is visible as GameState's
// connection-state machine advances. Individual panels only handle
// their own screen's input; every cross-cutting transition lives here.
// The node tree (World + UI CanvasLayer with the four panels) lives in
// Main.tscn.
public partial class Main : Node
{
    // World Zero has no client-auto-joined channel of its own (§10 —
    // `chat.yaml` system channels get *created* at server startup, but a
    // client connection still has to explicitly Join to attach to any
    // of them). Auto-joining a well-known name here on world-join is
    // this test grounds' own convenience so two clients can chat
    // immediately without both typing/clicking Join first — any name
    // works, since `Join` finds-or-creates a `group` channel by that
    // exact name regardless (§10).
    private static readonly string[] DefaultChatChannels = { "general" };

    private LoginPanel _loginPanel = null!;
    private RealmSelectPanel _realmSelectPanel = null!;
    private CharacterSelectPanel _characterSelectPanel = null!;
    private Hud _hud = null!;
    private WorldController _world = null!;

    public override void _Ready()
    {
        _world = GetNode<WorldController>("%World");
        _loginPanel = GetNode<LoginPanel>("%LoginPanel");
        _realmSelectPanel = GetNode<RealmSelectPanel>("%RealmSelectPanel");
        _characterSelectPanel = GetNode<CharacterSelectPanel>("%CharacterSelectPanel");
        _hud = GetNode<Hud>("%Hud");

        // #307: one shared Theme, assigned to each top-level panel
        // individually — the UI CanvasLayer is a CanvasLayer, not a
        // Control, so there's no single ancestor to set `.Theme` on
        // once for every panel to inherit from.
        _loginPanel.Theme = AppTheme.Instance;
        _realmSelectPanel.Theme = AppTheme.Instance;
        _characterSelectPanel.Theme = AppTheme.Instance;
        _hud.Theme = AppTheme.Instance;

        GameState.Instance.ConnectionStateChanged += OnConnectionStateChanged;

        var nc = NetworkClient.Instance;
        nc.OnAuthenticated += HandleAuthenticated;
        nc.OnRealmSelected += HandleRealmSelected;
        nc.OnCharacterSelected += HandleCharacterSelected;
    }

    private void HandleAuthenticated(Wire.Auth.Authenticated msg)
    {
        var gs = GameState.Instance;
        gs.AccountId = msg.AccountId;
        gs.Username = msg.Username;
        gs.SessionToken = msg.SessionToken;
        SessionStore.Save(msg.Username, msg.SessionToken);

        gs.SetConnectionState(ConnectionState.RealmSelect);
        _realmSelectPanel.RefreshOnShow();
    }

    private void HandleRealmSelected(Wire.Realm.RealmSelected msg)
    {
        var gs = GameState.Instance;
        gs.RealmId = msg.RealmId;
        gs.SetConnectionState(ConnectionState.CharacterSelect);
        _characterSelectPanel.RefreshOnShow();
    }

    private void HandleCharacterSelected(Wire.Character.CharacterSelected msg)
    {
        var gs = GameState.Instance;
        gs.CharacterId = msg.CharacterId;
        gs.ResetForNewCharacter();
        // World-join happens automatically right after this (§2.5 step
        // 4) — the `Joined` message that follows is what WorldController
        // actually reacts to; this just unblocks the 3D view/HUD.
        gs.SetConnectionState(ConnectionState.InWorld);

        foreach (var channel in DefaultChatChannels)
        {
            NetworkClient.Instance.SendChatJoin(channel);
        }
    }

    private void OnConnectionStateChanged(ConnectionState state)
    {
        _loginPanel.Visible = state is ConnectionState.Disconnected or ConnectionState.Connecting or ConnectionState.Authenticating;
        _realmSelectPanel.Visible = state is ConnectionState.RealmSelect;
        _characterSelectPanel.Visible = state is ConnectionState.CharacterSelect;
        _hud.Visible = state is ConnectionState.InWorld;
        _world.Visible = state is ConnectionState.InWorld;
    }
}
