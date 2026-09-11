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
// just a client-side-cosmetic choice). Layout lives in
// CharacterSelectPanel.tscn.
public partial class CharacterSelectPanel : Control
{
    private ItemList _characterList = null!;
    private LineEdit _newNameEdit = null!;
    private OptionButton _archetypeOption = null!;
    private Label _statusLabel = null!;
    private string[] _archetypeKeys = System.Array.Empty<string>();
    private CharacterList? _lastList;
    private ConfirmationDialog _deleteConfirm = null!;
    private string? _pendingDeleteCharacterId;

    public override void _Ready()
    {
        _characterList = GetNode<ItemList>("%CharacterList");
        _newNameEdit = GetNode<LineEdit>("%NewNameEdit");
        _archetypeOption = GetNode<OptionButton>("%ArchetypeOption");
        _statusLabel = GetNode<Label>("%StatusLabel");
        _deleteConfirm = GetNode<ConfirmationDialog>("%DeleteConfirm");
        _deleteConfirm.Confirmed += OnDeleteConfirmed;

        GetNode<Button>("%SelectButton").Pressed += OnSelectHighlighted;
        GetNode<Button>("%DeleteButton").Pressed += OnDeletePressed;
        GetNode<Button>("%CreateButton").Pressed += OnCreatePressed;
        GetNode<Button>("%AutoButton").Pressed += OnAutoPressed;
        GetNode<Button>("%BackButton").Pressed += OnBackToRealmSelectPressed;
        GetNode<Button>("%DisconnectButton").Pressed += () => NetworkClient.Instance.Disconnect();

        var nc = NetworkClient.Instance;
        nc.OnCharacterList += HandleCharacterList;
        nc.OnCharacterOptions += HandleCharacterOptions;
        nc.OnCharacterCreated += created => nc.SendSelectCharacter(created.CharacterId);
        nc.OnCharacterError += HandleCharacterError;
        nc.OnCharacterDeleted += _ => NetworkClient.Instance.SendListCharacters();
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

    private void OnDeletePressed()
    {
        var selected = _characterList.GetSelectedItems();
        if (selected.Length == 0 || _lastList is null)
        {
            _statusLabel.Text = "Highlight a character first.";
            return;
        }
        var character = _lastList.Characters[selected[0]];
        _pendingDeleteCharacterId = character.CharacterId;
        _deleteConfirm.DialogText = $"Permanently delete \"{character.Name}\"? This cannot be undone.";
        _deleteConfirm.PopupCentered();
    }

    private void OnDeleteConfirmed()
    {
        if (_pendingDeleteCharacterId is { } characterId)
        {
            NetworkClient.Instance.SendDeleteCharacter(characterId);
            _pendingDeleteCharacterId = null;
        }
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
