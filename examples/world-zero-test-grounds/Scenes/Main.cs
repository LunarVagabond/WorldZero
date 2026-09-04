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
        _world = new WorldController { Name = "World" };
        AddChild(_world);

        var uiLayer = new CanvasLayer { Name = "UI" };
        AddChild(uiLayer);

        _loginPanel = new LoginPanel { Name = "LoginPanel" };
        uiLayer.AddChild(_loginPanel);

        _realmSelectPanel = new RealmSelectPanel { Name = "RealmSelectPanel", Visible = false };
        uiLayer.AddChild(_realmSelectPanel);

        _characterSelectPanel = new CharacterSelectPanel { Name = "CharacterSelectPanel", Visible = false };
        uiLayer.AddChild(_characterSelectPanel);

        _hud = new Hud { Name = "Hud", Visible = false };
        uiLayer.AddChild(_hud);

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
