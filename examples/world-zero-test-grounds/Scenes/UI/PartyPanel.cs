using Godot;
using WorldZeroTestGrounds.Net;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes.UI;

// Real party invite/accept/decline/leave (PROMPT.md §11, §18 step 13).
// A target is addressed by live entity id — sourced either by typing it
// in or by clicking a roster entry in the 3D view first (§7.3's
// "targeting is entirely client-side" pattern reused here). Layout
// lives in PartyPanel.tscn.
public partial class PartyPanel : Control
{
    private LineEdit _targetEdit = null!;
    private Label _pendingInviteLabel = null!;
    private Label _membersLabel = null!;

    public override void _Ready()
    {
        _targetEdit = GetNode<LineEdit>("%TargetEdit");
        _pendingInviteLabel = GetNode<Label>("%PendingInviteLabel");
        _membersLabel = GetNode<Label>("%MembersLabel");

        UiHelpers.LockMovementWhileFocused(_targetEdit);
        GetNode<Button>("%UseTargetButton").Pressed += () => _targetEdit.Text = GameState.Instance.CurrentTargetEntityId ?? "";
        GetNode<Button>("%InviteButton").Pressed += () => NetworkClient.Instance.SendPartyInvite(_targetEdit.Text.Trim());

        GetNode<Button>("%AcceptButton").Pressed += () =>
        {
            NetworkClient.Instance.SendPartyInviteResponse(true);
            _pendingInviteLabel.Text = "(no pending invite)";
        };
        GetNode<Button>("%DeclineButton").Pressed += () =>
        {
            NetworkClient.Instance.SendPartyInviteResponse(false);
            _pendingInviteLabel.Text = "(no pending invite)";
        };

        GetNode<Button>("%JoinLayerButton").Pressed += () => NetworkClient.Instance.SendJoinGroupLayer(_targetEdit.Text.Trim());
        GetNode<Button>("%LeaveButton").Pressed += () => NetworkClient.Instance.SendPartyLeave();

        var nc = NetworkClient.Instance;
        nc.OnPartyInviteReceived += msg => _pendingInviteLabel.Text = $"Invite from {msg.FromEntityId}";
        nc.OnPartyUpdate += msg =>
        {
            GameState.Instance.PartyMembers.Clear();
            GameState.Instance.PartyMembers.AddRange(msg.Members);
            _membersLabel.Text = msg.Members.Count == 0 ? "(no party)" : string.Join("\n", msg.Members);
        };
    }
}
