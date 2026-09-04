using Godot;
using WorldZeroTestGrounds.Net;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes.UI;

// Register/Login/Resume form (PROMPT.md §2.4, §18 steps 1-2/17). Built
// entirely in code — this whole project favors code-built Control trees
// over hand-authored .tscn node graphs, since there's no interactive
// Godot editor session available while building this. Split into small
// named Build*Section methods (each returning nothing, just populating
// fields) rather than one long _Ready() — the previous version was one
// unbroken 60-line method mixing layout, wiring, and copy together.
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
        SetAnchorsPreset(LayoutPreset.FullRect);

        var panel = new PanelContainer();
        panel.SetAnchorsPreset(LayoutPreset.Center);
        AddChild(panel);

        var root = new VBoxContainer { CustomMinimumSize = new Vector2(380, 0) };
        panel.AddChild(root);

        root.AddChild(new Label { Text = "World Zero Test Grounds", HorizontalAlignment = HorizontalAlignment.Center });
        BuildMultiClientWarning(root);
        BuildConnectionSection(root);
        BuildCredentialsSection(root);
        BuildResumeSection(root);
        BuildStatusLabel(root);

        NetworkClient.Instance.OnAuthError += msg => _statusLabel.Text = $"Error: {msg}";
        NetworkClient.Instance.OnDisconnected += reason => _statusLabel.Text = $"Disconnected: {reason}";
    }

    private static void BuildMultiClientWarning(Control parent)
    {
        UiHelpers.AddWrappingLabel(parent,
            "Running a second client on this machine? Register a NEW account below for it — the server hard-blocks selecting the same character from two connections at once, so reusing this one's default credentials will get the other client stuck.")
            .Modulate = new Color(0.9f, 0.85f, 0.4f);
    }

    private void BuildConnectionSection(Control parent)
    {
        var section = UiHelpers.Section(parent, "Server");
        var env = EnvConfig.Instance;

        section.AddChild(new Label { Text = "Host" });
        _hostEdit = new LineEdit { Text = env.ServerHost };
        section.AddChild(_hostEdit);

        section.AddChild(new Label { Text = "Port" });
        _portEdit = new LineEdit { Text = env.ServerPort.ToString() };
        section.AddChild(_portEdit);
    }

    private void BuildCredentialsSection(Control parent)
    {
        var section = UiHelpers.Section(parent, "Account");
        var env = EnvConfig.Instance;

        section.AddChild(new Label { Text = "Username" });
        _userEdit = new LineEdit { Text = env.DefaultUsername };
        section.AddChild(_userEdit);

        section.AddChild(new Label { Text = "Password" });
        _passEdit = new LineEdit { Text = env.DefaultPassword, Secret = true };
        section.AddChild(_passEdit);

        var buttonRow = new HBoxContainer();
        section.AddChild(buttonRow);

        var registerButton = new Button { Text = "Register", SizeFlagsHorizontal = SizeFlags.ExpandFill };
        registerButton.Pressed += () => OnSubmit(Mode.Register);
        buttonRow.AddChild(registerButton);

        var loginButton = new Button { Text = "Login", SizeFlagsHorizontal = SizeFlags.ExpandFill };
        loginButton.Pressed += () => OnSubmit(Mode.Login);
        buttonRow.AddChild(loginButton);
    }

    private void BuildResumeSection(Control parent)
    {
        var section = UiHelpers.Section(parent, "Resume a saved account");
        UiHelpers.AddWrappingLabel(section, "Stored per-username, not \"last session\" — safe to use even with another client window open on this machine.");

        var resumeRow = new HBoxContainer();
        section.AddChild(resumeRow);
        _savedSessions = new OptionButton { SizeFlagsHorizontal = SizeFlags.ExpandFill };
        resumeRow.AddChild(_savedSessions);
        _resumeButton = new Button { Text = "Resume" };
        _resumeButton.Pressed += () => OnSubmit(Mode.Resume);
        resumeRow.AddChild(_resumeButton);
        RefreshSavedSessions();
    }

    private void BuildStatusLabel(Control parent)
    {
        _statusLabel = UiHelpers.AddWrappingLabel(parent);
        _statusLabel.Modulate = new Color(1f, 0.35f, 0.35f);
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
