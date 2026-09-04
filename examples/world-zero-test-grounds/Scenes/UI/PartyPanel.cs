using Godot;
using WorldZeroTestGrounds.Net;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes.UI;

// Real party invite/accept/decline/leave (PROMPT.md §11, §18 step 13).
// A target is addressed by live entity id — sourced either by typing it
// in or by clicking a roster entry in the 3D view first (§7.3's
// "targeting is entirely client-side" pattern reused here).
public partial class PartyPanel : Control
{
    private LineEdit _targetEdit = null!;
    private Label _pendingInviteLabel = null!;
    private string? _pendingInviteFrom;

    public override void _Ready()
    {
        SetAnchorsPreset(LayoutPreset.FullRect);
        var box = UiHelpers.CreateScrollableColumn(this);

        UiHelpers.AddWrappingLabel(box, "Target entity id (or click an entity in the 3D view, then Use Target):");
        var targetRow = new HBoxContainer();
        box.AddChild(targetRow);
        _targetEdit = new LineEdit { SizeFlagsHorizontal = SizeFlags.ExpandFill };
        UiHelpers.LockMovementWhileFocused(_targetEdit);
        targetRow.AddChild(_targetEdit);
        var useTargetButton = new Button { Text = "Use current target" };
        useTargetButton.Pressed += () => _targetEdit.Text = GameState.Instance.CurrentTargetEntityId ?? "";
        targetRow.AddChild(useTargetButton);

        var inviteButton = new Button { Text = "Invite" };
        inviteButton.Pressed += () => NetworkClient.Instance.SendPartyInvite(_targetEdit.Text.Trim());
        box.AddChild(inviteButton);

        box.AddChild(new HSeparator());
        _pendingInviteLabel = UiHelpers.AddWrappingLabel(box, "(no pending invite)");
        var respondRow = new HBoxContainer();
        box.AddChild(respondRow);
        var acceptButton = new Button { Text = "Accept" };
        acceptButton.Pressed += () => { NetworkClient.Instance.SendPartyInviteResponse(true); _pendingInviteFrom = null; _pendingInviteLabel.Text = "(no pending invite)"; };
        respondRow.AddChild(acceptButton);
        var declineButton = new Button { Text = "Decline" };
        declineButton.Pressed += () => { NetworkClient.Instance.SendPartyInviteResponse(false); _pendingInviteFrom = null; _pendingInviteLabel.Text = "(no pending invite)"; };
        respondRow.AddChild(declineButton);

        box.AddChild(new HSeparator());
        var leaveButton = new Button { Text = "Leave party" };
        leaveButton.Pressed += () => NetworkClient.Instance.SendPartyLeave();
        box.AddChild(leaveButton);

        var nc = NetworkClient.Instance;
        nc.OnPartyInviteReceived += msg =>
        {
            _pendingInviteFrom = msg.FromEntityId;
            _pendingInviteLabel.Text = $"Invite from {msg.FromEntityId}";
        };
        nc.OnPartyUpdate += msg =>
        {
            GameState.Instance.PartyMembers.Clear();
            GameState.Instance.PartyMembers.AddRange(msg.Members);
        };
    }
}
