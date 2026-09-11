using Godot;
using WorldZeroTestGrounds.Net;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes.UI;

// Real guild create/invite/accept/leave/disband/kick/promote/demote/
// motd/tag (PROMPT.md §12, §18 step 14). Layout lives in GuildPanel.tscn.
public partial class GuildPanel : Control
{
    private LineEdit _targetEdit = null!;
    private Label _pendingInviteLabel = null!;

    public override void _Ready()
    {
        var createNameEdit = GetNode<LineEdit>("%CreateNameEdit");
        UiHelpers.LockMovementWhileFocused(createNameEdit);
        GetNode<Button>("%CreateButton").Pressed += () => NetworkClient.Instance.SendGuildCreate(createNameEdit.Text.Trim());

        _targetEdit = GetNode<LineEdit>("%TargetEdit");
        UiHelpers.LockMovementWhileFocused(_targetEdit);
        GetNode<Button>("%UseTargetButton").Pressed += () => _targetEdit.Text = GameState.Instance.CurrentTargetEntityId ?? "";

        GetNode<Button>("%InviteButton").Pressed += () => NetworkClient.Instance.SendGuildInvite(_targetEdit.Text.Trim());
        GetNode<Button>("%KickButton").Pressed += () => NetworkClient.Instance.SendGuildKick(_targetEdit.Text.Trim());

        var rankKeyEdit = GetNode<LineEdit>("%RankKeyEdit");
        UiHelpers.LockMovementWhileFocused(rankKeyEdit);
        GetNode<Button>("%PromoteButton").Pressed += () => NetworkClient.Instance.SendGuildPromote(_targetEdit.Text.Trim(), rankKeyEdit.Text.Trim());
        GetNode<Button>("%DemoteButton").Pressed += () => NetworkClient.Instance.SendGuildDemote(_targetEdit.Text.Trim(), rankKeyEdit.Text.Trim());

        _pendingInviteLabel = GetNode<Label>("%PendingInviteLabel");
        GetNode<Button>("%AcceptButton").Pressed += () =>
        {
            NetworkClient.Instance.SendGuildInviteResponse(true);
            _pendingInviteLabel.Text = "(no pending invite)";
        };
        GetNode<Button>("%DeclineButton").Pressed += () =>
        {
            NetworkClient.Instance.SendGuildInviteResponse(false);
            _pendingInviteLabel.Text = "(no pending invite)";
        };

        var motdEdit = GetNode<LineEdit>("%MotdEdit");
        UiHelpers.LockMovementWhileFocused(motdEdit);
        GetNode<Button>("%MotdButton").Pressed += () => NetworkClient.Instance.SendGuildSetMotd(motdEdit.Text);

        var tagEdit = GetNode<LineEdit>("%TagEdit");
        UiHelpers.LockMovementWhileFocused(tagEdit);
        GetNode<Button>("%TagButton").Pressed += () => NetworkClient.Instance.SendGuildSetTag(tagEdit.Text);

        GetNode<Button>("%LeaveButton").Pressed += () => NetworkClient.Instance.SendGuildLeave();
        GetNode<Button>("%DisbandButton").Pressed += () => NetworkClient.Instance.SendGuildDisband();

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
