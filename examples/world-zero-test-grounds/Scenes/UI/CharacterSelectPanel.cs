using Godot;
using WorldZeroTestGrounds.Net;
using WorldZeroTestGrounds.State;
using WorldZeroTestGrounds.Wire.Character;

namespace WorldZeroTestGrounds.Scenes.UI;

// ListCharacters/CreateCharacter/SelectCharacter (PROMPT.md §3.2, §18
// steps 4-5) plus a real archetype picker against ListCharacterOptions/
// CharacterOptions — the upgrade this project's plan found in the
// actual `character.proto` beyond what PROMPT.md §3.3 described (a
// real "ask for options, let the player pick" flow exists now, not
// just a client-side-cosmetic choice). Split into small named
// Build*Section methods rather than one long _Ready(), same reasoning
// as LoginPanel.
public partial class CharacterSelectPanel : Control
{
    private ItemList _characterList = null!;
    private LineEdit _newNameEdit = null!;
    private OptionButton _archetypeOption = null!;
    private Label _statusLabel = null!;
    private string[] _archetypeKeys = System.Array.Empty<string>();
    private CharacterList? _lastList;

    public override void _Ready()
    {
        SetAnchorsPreset(LayoutPreset.FullRect);

        var panel = new PanelContainer();
        panel.SetAnchorsPreset(LayoutPreset.Center);
        AddChild(panel);

        var root = new VBoxContainer { CustomMinimumSize = new Vector2(420, 0) };
        panel.AddChild(root);

        root.AddChild(new Label { Text = "Select a character", HorizontalAlignment = HorizontalAlignment.Center });
        BuildTwoClientWarning(root);
        BuildStatusLabel(root);
        BuildExistingCharactersSection(root);
        BuildCreateCharacterSection(root);
        BuildUtilitySection(root);

        var nc = NetworkClient.Instance;
        nc.OnCharacterList += HandleCharacterList;
        nc.OnCharacterOptions += HandleCharacterOptions;
        nc.OnCharacterCreated += created => nc.SendSelectCharacter(created.CharacterId);
        nc.OnCharacterError += HandleCharacterError;
    }

    private static void BuildTwoClientWarning(Control parent)
    {
        // World Zero already refuses to let the same character be
        // SelectCharacter'd from two connections at once (real
        // server-side blocking, `realm-directory`'s login_policy — a
        // second client using the SAME account gets a hard Error here,
        // not a silent success). That error used to render into a tiny
        // unstyled label at the very bottom of this panel, easy to miss
        // entirely — from the outside it just looked like the second
        // client's "Auto" button did nothing. Made loud and explicit.
        UiHelpers.AddWrappingLabel(parent,
            "Testing with two clients? Each one needs a DIFFERENT account — click Register on each, don't Login/Resume the same account twice.")
            .Modulate = new Color(0.9f, 0.85f, 0.4f);
    }

    private void BuildStatusLabel(Control parent)
    {
        _statusLabel = UiHelpers.AddWrappingLabel(parent);
        _statusLabel.Modulate = new Color(1f, 0.35f, 0.35f);
    }

    private void BuildExistingCharactersSection(Control parent)
    {
        var section = UiHelpers.Section(parent, "Your characters");
        _characterList = new ItemList { CustomMinimumSize = new Vector2(0, 120) };
        section.AddChild(_characterList);

        var selectButton = new Button { Text = "Select highlighted" };
        selectButton.Pressed += OnSelectHighlighted;
        section.AddChild(selectButton);
    }

    private void BuildCreateCharacterSection(Control parent)
    {
        var section = UiHelpers.Section(parent, "Create new character");

        section.AddChild(new Label { Text = "Name" });
        _newNameEdit = new LineEdit { PlaceholderText = "Character name" };
        section.AddChild(_newNameEdit);

        section.AddChild(new Label { Text = "Archetype (real server-declared options, §3.3)" });
        _archetypeOption = new OptionButton();
        section.AddChild(_archetypeOption);

        var createButton = new Button { Text = "Create + select" };
        createButton.Pressed += OnCreatePressed;
        section.AddChild(createButton);
    }

    private void BuildUtilitySection(Control parent)
    {
        var section = UiHelpers.Section(parent, "");
        var autoButton = new Button { Text = "Auto: pick first, or create if none" };
        autoButton.Pressed += OnAutoPressed;
        section.AddChild(autoButton);

        // There is no in-session "go back" — the wire protocol's
        // realm(2)->character(3) handshake is strictly forward-only
        // (server.rs's `handle_session`: a SelectRealm sent after the
        // realm phase has ended fails to parse as the expected
        // message_type and the server closes the connection). So
        // "back to realm select" has to be a real reconnect: disconnect,
        // open a fresh socket, and Resume the same session token —
        // Resume replies with Authenticated same as Login, which
        // Main.cs already routes into RealmSelect.
        var backButton = new Button { Text = "Back to realm select" };
        backButton.Pressed += OnBackToRealmSelectPressed;
        section.AddChild(backButton);

        var disconnectButton = new Button { Text = "Disconnect (use a different account)" };
        disconnectButton.Pressed += () => NetworkClient.Instance.Disconnect();
        section.AddChild(disconnectButton);
    }

    private async void OnBackToRealmSelectPressed()
    {
        string? token = GameState.Instance.SessionToken;
        if (string.IsNullOrEmpty(token))
        {
            _statusLabel.Text = "No session token cached for this connection — can't reconnect. Disconnect and log in again instead.";
            return;
        }

        _statusLabel.Text = "Reconnecting...";
        NetworkClient.Instance.Disconnect();
        var gs = GameState.Instance;
        gs.RealmId = null;
        gs.CharacterId = null;
        gs.EntityId = null;
        gs.ZoneId = null;
        gs.ResetForNewCharacter();
        var env = EnvConfig.Instance;
        bool ok = await NetworkClient.Instance.ConnectAsync(env.ServerHost, env.ServerPort);
        if (!ok)
        {
            _statusLabel.Text = "Reconnect failed — is `server` still running?";
            return;
        }

        GameState.Instance.SetConnectionState(ConnectionState.Authenticating);
        NetworkClient.Instance.SendResume(token);
    }

    private void HandleCharacterError(string msg)
    {
        _statusLabel.Text = msg.Contains("already logged in elsewhere")
            ? $"BLOCKED: {msg}\nThis account/character is already active on another connection. Go back and Register a separate account for this client instead."
            : $"Error: {msg}";
    }

    private void HandleCharacterList(CharacterList list)
    {
        _lastList = list;
        _characterList.Clear();
        foreach (var c in list.Characters)
        {
            _characterList.AddItem($"{c.Name}  [{c.ZoneId}]  ({c.CharacterId})");
        }
    }

    private void HandleCharacterOptions(CharacterOptions options)
    {
        _archetypeOption.Clear();
        _archetypeKeys = new string[options.Archetypes.Count];
        for (int i = 0; i < options.Archetypes.Count; i++)
        {
            var a = options.Archetypes[i];
            _archetypeKeys[i] = a.Key;
            _archetypeOption.AddItem(string.IsNullOrEmpty(a.Description) ? a.Name : $"{a.Name} — {a.Description}");
        }
    }

    private void OnSelectHighlighted()
    {
        var selected = _characterList.GetSelectedItems();
        if (selected.Length == 0 || _lastList is null)
        {
            _statusLabel.Text = "Highlight a character first.";
            return;
        }
        var characterId = _lastList.Characters[selected[0]].CharacterId;
        NetworkClient.Instance.SendSelectCharacter(characterId);
    }

    private void OnCreatePressed()
    {
        string name = _newNameEdit.Text.Trim();
        if (name.Length == 0)
        {
            _statusLabel.Text = "Name required.";
            return;
        }
        string archetype = _archetypeOption.Selected >= 0 && _archetypeOption.Selected < _archetypeKeys.Length
            ? _archetypeKeys[_archetypeOption.Selected]
            : "";
        NetworkClient.Instance.SendCreateCharacter(name, archetype);
    }

    private void OnAutoPressed()
    {
        if (_lastList is { Characters.Count: > 0 })
        {
            NetworkClient.Instance.SendSelectCharacter(_lastList.Characters[0].CharacterId);
        }
        else
        {
            NetworkClient.Instance.SendCreateCharacter($"tester-{System.DateTime.UtcNow:HHmmss}", "");
        }
    }

    public void RefreshOnShow()
    {
        NetworkClient.Instance.SendListCharacters();
        NetworkClient.Instance.SendListCharacterOptions();
    }
}
