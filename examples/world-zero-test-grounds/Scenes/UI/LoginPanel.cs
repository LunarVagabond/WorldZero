using Godot;
using WorldZeroTestGrounds.Net;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes.UI;

// Register/Login/Resume form (PROMPT.md §2.4, §18 steps 1-2/17). Layout
// lives in LoginPanel.tscn; this script only wires signals and behavior.
public partial class LoginPanel : Control
{
    private LineEdit _hostEdit = null!;
    private LineEdit _portEdit = null!;
    private LineEdit _userEdit = null!;
    private LineEdit _passEdit = null!;
    private Label _statusLabel = null!;
    private OptionButton _savedSessions = null!;
    private Button _resumeButton = null!;
    private string[] _savedUsernames = System.Array.Empty<string>();

    public override void _Ready()
    {
        _hostEdit = GetNode<LineEdit>("%HostEdit");
        _portEdit = GetNode<LineEdit>("%PortEdit");
        _userEdit = GetNode<LineEdit>("%UserEdit");
        _passEdit = GetNode<LineEdit>("%PassEdit");
        _statusLabel = GetNode<Label>("%StatusLabel");
        _savedSessions = GetNode<OptionButton>("%SavedSessions");
        _resumeButton = GetNode<Button>("%ResumeButton");

        var env = EnvConfig.Instance;
        _hostEdit.Text = env.ServerHost;
        _portEdit.Text = env.ServerPort.ToString();
        _userEdit.Text = env.DefaultUsername;
        _passEdit.Text = env.DefaultPassword;

        GetNode<Button>("%RegisterButton").Pressed += () => OnSubmit(Mode.Register);
        GetNode<Button>("%LoginButton").Pressed += () => OnSubmit(Mode.Login);
        _resumeButton.Pressed += () => OnSubmit(Mode.Resume);

        RefreshSavedSessions();

        NetworkClient.Instance.OnAuthError += msg => _statusLabel.Text = $"Error: {msg}";
        NetworkClient.Instance.OnDisconnected += reason => _statusLabel.Text = $"Disconnected: {reason}";
    }

    private void RefreshSavedSessions()
    {
        var saved = SessionStore.LoadAll();
        _savedUsernames = new string[saved.Count];
        _savedSessions.Clear();
        int i = 0;
        foreach (var username in saved.Keys)
        {
            _savedUsernames[i] = username;
            _savedSessions.AddItem(username);
            i++;
        }
        _resumeButton.Disabled = saved.Count == 0;
        if (saved.Count > 0)
        {
            _savedSessions.Selected = 0;
        }
    }

    private enum Mode { Register, Login, Resume }

    private async void OnSubmit(Mode mode)
    {
        if (!int.TryParse(_portEdit.Text, out int port))
        {
            _statusLabel.Text = "Invalid port";
            return;
        }

        _statusLabel.Text = "Connecting...";
        GameState.Instance.SetConnectionState(ConnectionState.Connecting);
        bool ok = await NetworkClient.Instance.ConnectAsync(_hostEdit.Text, port);
        if (!ok)
        {
            _statusLabel.Text = "Connect failed — is `server` running? See README.";
            return;
        }

        GameState.Instance.SetConnectionState(ConnectionState.Authenticating);
        _statusLabel.Text = "Authenticating...";

        switch (mode)
        {
            case Mode.Register:
                NetworkClient.Instance.SendRegister(_userEdit.Text, _passEdit.Text);
                break;
            case Mode.Login:
                NetworkClient.Instance.SendLogin(_userEdit.Text, _passEdit.Text);
                break;
            case Mode.Resume:
                SubmitResume();
                break;
        }
    }

    private void SubmitResume()
    {
        if (_savedSessions.Selected < 0 || _savedSessions.Selected >= _savedUsernames.Length)
        {
            _statusLabel.Text = "No saved session selected.";
            return;
        }
        string selectedUsername = _savedUsernames[_savedSessions.Selected];
        var allSaved = SessionStore.LoadAll();
        if (!allSaved.TryGetValue(selectedUsername, out var token))
        {
            _statusLabel.Text = "That saved session is gone — try again.";
            return;
        }
        _statusLabel.Text = $"Resuming {selectedUsername}...";
        NetworkClient.Instance.SendResume(token);
    }
}
