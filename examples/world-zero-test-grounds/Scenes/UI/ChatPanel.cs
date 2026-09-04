using Godot;
using WorldZeroTestGrounds.Net;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes.UI;

// Join-by-name group chat (PROMPT.md §10, §18 step 12). channel_id is
// resolved from the Joined reply and never re-derivable except by
// re-joining (§14) — this panel keeps the name->id map in GameState so
// re-selecting a previously joined channel doesn't need a fresh Join.
public partial class ChatPanel : Control
{
    private LineEdit _channelNameEdit = null!;
    private OptionButton _joinedChannels = null!;
    private LineEdit _messageEdit = null!;
    private RichTextLabel _log = null!;

    public override void _Ready()
    {
        SetAnchorsPreset(LayoutPreset.FullRect);
        var box = UiHelpers.CreateScrollableColumn(this);

        var joinRow = new HBoxContainer();
        box.AddChild(joinRow);
        _channelNameEdit = new LineEdit { PlaceholderText = "channel name", CustomMinimumSize = new Vector2(150, 0) };
        UiHelpers.LockMovementWhileFocused(_channelNameEdit);
        joinRow.AddChild(_channelNameEdit);
        var joinButton = new Button { Text = "Join" };
        joinButton.Pressed += () => NetworkClient.Instance.SendChatJoin(_channelNameEdit.Text.Trim());
        joinRow.AddChild(joinButton);
        var leaveButton = new Button { Text = "Leave selected" };
        leaveButton.Pressed += OnLeaveSelected;
        joinRow.AddChild(leaveButton);

        box.AddChild(new Label { Text = "Active channel (to send to):" });
        _joinedChannels = new OptionButton();
        box.AddChild(_joinedChannels);

        _log = new RichTextLabel { ScrollFollowing = true, SizeFlagsVertical = SizeFlags.ExpandFill, CustomMinimumSize = new Vector2(0, 150) };
        box.AddChild(_log);

        var sendRow = new HBoxContainer();
        box.AddChild(sendRow);
        _messageEdit = new LineEdit { PlaceholderText = "message", SizeFlagsHorizontal = SizeFlags.ExpandFill };
        UiHelpers.LockMovementWhileFocused(_messageEdit);
        _messageEdit.TextSubmitted += _ => OnSend();
        sendRow.AddChild(_messageEdit);
        var sendButton = new Button { Text = "Send" };
        sendButton.Pressed += OnSend;
        sendRow.AddChild(sendButton);

        var nc = NetworkClient.Instance;
        nc.OnChatJoined += msg => RefreshChannelOptions(msg.Channel);
        nc.OnChatLeft += _ => RefreshChannelOptions();
        nc.OnChatMessage += msg => _log.AppendText($"[{msg.Channel}] {msg.Sender}: {msg.Body}\n");
        nc.OnChatError += msg => _log.AppendText($"[error] {msg}\n");
    }

    // Rebuilding the OptionButton (Clear() + AddItem()) does NOT keep
    // any item selected — Send silently no-op'd with "join and select a
    // channel first" left sitting in this panel's own small log, easy to
    // miss, which is why chat looked broken even right after joining.
    // Now explicitly reselects: the just-joined channel if one is named,
    // else whatever was selected before, else the most recently joined.
    private void RefreshChannelOptions(string? preferred = null)
    {
        string? previousSelected = _joinedChannels.Selected >= 0 ? _joinedChannels.GetItemText(_joinedChannels.Selected) : null;
        _joinedChannels.Clear();
        int selectIdx = -1;
        int i = 0;
        foreach (var name in GameState.Instance.ChatChannels.Keys)
        {
            _joinedChannels.AddItem(name);
            if (name == preferred || (preferred is null && name == previousSelected))
            {
                selectIdx = i;
            }
            i++;
        }
        if (selectIdx < 0 && _joinedChannels.ItemCount > 0)
        {
            selectIdx = _joinedChannels.ItemCount - 1;
        }
        if (selectIdx >= 0)
        {
            _joinedChannels.Selected = selectIdx;
        }
    }

    private void OnLeaveSelected()
    {
        if (_joinedChannels.Selected < 0)
        {
            return;
        }
        string name = _joinedChannels.GetItemText(_joinedChannels.Selected);
        NetworkClient.Instance.SendChatLeave(name);
    }

    private void OnSend()
    {
        if (_joinedChannels.Selected < 0)
        {
            _log.AppendText("[error] join and select a channel first\n");
            return;
        }
        string name = _joinedChannels.GetItemText(_joinedChannels.Selected);
        if (!GameState.Instance.ChatChannels.TryGetValue(name, out var channelId))
        {
            return;
        }
        string body = _messageEdit.Text.Trim();
        if (body.Length == 0)
        {
            return;
        }
        NetworkClient.Instance.SendChatSend(channelId, body);
        _messageEdit.Text = "";
    }
}
