using Godot;
using WorldZeroTestGrounds.Net;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes.UI;

// Real guild create/invite/accept/leave/disband/kick/promote/demote/
// motd/tag (PROMPT.md §12, §18 step 14).
public partial class GuildPanel : Control
{
    private LineEdit _createNameEdit = null!;
    private LineEdit _targetEdit = null!;
    private LineEdit _rankKeyEdit = null!;
    private LineEdit _motdEdit = null!;
    private LineEdit _tagEdit = null!;
    private Label _pendingInviteLabel = null!;

    public override void _Ready()
    {
        SetAnchorsPreset(LayoutPreset.FullRect);
        var box = UiHelpers.CreateScrollableColumn(this);

        var createRow = new HBoxContainer();
        box.AddChild(createRow);
        _createNameEdit = new LineEdit { PlaceholderText = "guild name", SizeFlagsHorizontal = SizeFlags.ExpandFill };
        UiHelpers.LockMovementWhileFocused(_createNameEdit);
        createRow.AddChild(_createNameEdit);
        var createButton = new Button { Text = "Create guild" };
        createButton.Pressed += () => NetworkClient.Instance.SendGuildCreate(_createNameEdit.Text.Trim());
        createRow.AddChild(createButton);

        box.AddChild(new HSeparator());
        UiHelpers.AddWrappingLabel(box, "Target entity id (invite/kick/promote/demote):");
        var targetRow = new HBoxContainer();
        box.AddChild(targetRow);
        _targetEdit = new LineEdit { SizeFlagsHorizontal = SizeFlags.ExpandFill };
        UiHelpers.LockMovementWhileFocused(_targetEdit);
        targetRow.AddChild(_targetEdit);
        var useTargetButton = new Button { Text = "Use current target" };
        useTargetButton.Pressed += () => _targetEdit.Text = GameState.Instance.CurrentTargetEntityId ?? "";
        targetRow.AddChild(useTargetButton);

        var actionRow1 = new HBoxContainer();
        box.AddChild(actionRow1);
        var inviteButton = new Button { Text = "Invite" };
        inviteButton.Pressed += () => NetworkClient.Instance.SendGuildInvite(_targetEdit.Text.Trim());
        actionRow1.AddChild(inviteButton);
        var kickButton = new Button { Text = "Kick" };
        kickButton.Pressed += () => NetworkClient.Instance.SendGuildKick(_targetEdit.Text.Trim());
        actionRow1.AddChild(kickButton);

        _rankKeyEdit = new LineEdit { PlaceholderText = "rank_key (for promote/demote)" };
        UiHelpers.LockMovementWhileFocused(_rankKeyEdit);
        box.AddChild(_rankKeyEdit);
        var actionRow2 = new HBoxContainer();
        box.AddChild(actionRow2);
        var promoteButton = new Button { Text = "Promote" };
        promoteButton.Pressed += () => NetworkClient.Instance.SendGuildPromote(_targetEdit.Text.Trim(), _rankKeyEdit.Text.Trim());
        actionRow2.AddChild(promoteButton);
        var demoteButton = new Button { Text = "Demote" };
        demoteButton.Pressed += () => NetworkClient.Instance.SendGuildDemote(_targetEdit.Text.Trim(), _rankKeyEdit.Text.Trim());
        actionRow2.AddChild(demoteButton);

        box.AddChild(new HSeparator());
        _pendingInviteLabel = UiHelpers.AddWrappingLabel(box, "(no pending invite)");
        var respondRow = new HBoxContainer();
        box.AddChild(respondRow);
        var acceptButton = new Button { Text = "Accept invite" };
        acceptButton.Pressed += () => { NetworkClient.Instance.SendGuildInviteResponse(true); _pendingInviteLabel.Text = "(no pending invite)"; };
        respondRow.AddChild(acceptButton);
        var declineButton = new Button { Text = "Decline invite" };
        declineButton.Pressed += () => { NetworkClient.Instance.SendGuildInviteResponse(false); _pendingInviteLabel.Text = "(no pending invite)"; };
        respondRow.AddChild(declineButton);

        box.AddChild(new HSeparator());
        var motdRow = new HBoxContainer();
        box.AddChild(motdRow);
        _motdEdit = new LineEdit { PlaceholderText = "motd", SizeFlagsHorizontal = SizeFlags.ExpandFill };
        UiHelpers.LockMovementWhileFocused(_motdEdit);
        motdRow.AddChild(_motdEdit);
        var motdButton = new Button { Text = "Set MOTD" };
        motdButton.Pressed += () => NetworkClient.Instance.SendGuildSetMotd(_motdEdit.Text);
        motdRow.AddChild(motdButton);

        var tagRow = new HBoxContainer();
        box.AddChild(tagRow);
        _tagEdit = new LineEdit { PlaceholderText = "tag", SizeFlagsHorizontal = SizeFlags.ExpandFill };
        UiHelpers.LockMovementWhileFocused(_tagEdit);
        tagRow.AddChild(_tagEdit);
        var tagButton = new Button { Text = "Set tag" };
        tagButton.Pressed += () => NetworkClient.Instance.SendGuildSetTag(_tagEdit.Text);
        tagRow.AddChild(tagButton);

        box.AddChild(new HSeparator());
        var leaveRow = new HBoxContainer();
        box.AddChild(leaveRow);
        var leaveButton = new Button { Text = "Leave guild" };
        leaveButton.Pressed += () => NetworkClient.Instance.SendGuildLeave();
        leaveRow.AddChild(leaveButton);
        var disbandButton = new Button { Text = "Disband guild" };
        disbandButton.Pressed += () => NetworkClient.Instance.SendGuildDisband();
        leaveRow.AddChild(disbandButton);

        var nc = NetworkClient.Instance;
        nc.OnGuildInviteReceived += msg => _pendingInviteLabel.Text = $"Invite from {msg.FromEntityId}";
        nc.OnGuildUpdate += HandleGuildUpdate;
        nc.OnGuildDisbanded += () => _pendingInviteLabel.Text = "(guild disbanded)";
    }

    private void HandleGuildUpdate(WorldZeroTestGrounds.Wire.Session.GuildUpdate msg)
    {
        var gs = GameState.Instance;
        bool none = string.IsNullOrEmpty(msg.GuildId);
        gs.GuildId = none ? null : msg.GuildId;
        gs.GuildName = none ? null : msg.Name;
        gs.GuildMotd = none ? null : msg.Motd;
        gs.GuildTag = none ? null : msg.Tag;
        gs.GuildMembers.Clear();
        gs.MyGuildRankKey = null;
        foreach (var m in msg.Members)
        {
            gs.GuildMembers.Add((m.EntityId, m.RankKey));
            if (m.EntityId == gs.EntityId)
            {
                gs.MyGuildRankKey = m.RankKey;
            }
        }
    }
}
