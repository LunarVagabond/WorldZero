using Godot;
using WorldZeroTestGrounds.Net;

namespace WorldZeroTestGrounds.Scenes.UI;

// Admin/QA-only commands, backed by evil-cube-plugin's real
// caller-role-gated chat commands (docs/specs/Auth_Spec.md's "Account
// roles" — the one real, backend-enforced privilege mechanism World Zero
// has; there is no core wire concept of "admin" beyond it). Gated on
// either the real `admin` role or the lighter-weight `qa` role (#307 —
// grant it via `make role ARGS="grant <username> qa"`, zero backend
// plumbing needed). This panel's own visibility (Hud.cs) is purely a UI
// convenience — every command below still gets independently re-checked
// server-side, so an account with neither role is blocked by the actual
// backend even if this panel were somehow shown to them. Layout lives
// in AdminPanel.tscn.
public partial class AdminPanel : Control
{
    public override void _Ready()
    {
        var itemEdit = GetNode<LineEdit>("%ItemEdit");
        UiHelpers.LockMovementWhileFocused(itemEdit);
        var qtyEdit = GetNode<LineEdit>("%QtyEdit");
        UiHelpers.LockMovementWhileFocused(qtyEdit);
        GetNode<Button>("%GrantButton").Pressed += () =>
            NetworkClient.Instance.SendPluginChatCommand($"/grant {itemEdit.Text.Trim()} {qtyEdit.Text.Trim()}");

        var keyEdit = GetNode<LineEdit>("%KeyEdit");
        UiHelpers.LockMovementWhileFocused(keyEdit);
        var amountEdit = GetNode<LineEdit>("%AmountEdit");
        UiHelpers.LockMovementWhileFocused(amountEdit);
        GetNode<Button>("%CurrencyGrantButton").Pressed += () =>
            NetworkClient.Instance.SendPluginChatCommand($"/grantcurrency {keyEdit.Text.Trim()} {amountEdit.Text.Trim()}");

        GetNode<Button>("%KillButton").Pressed += () => NetworkClient.Instance.SendPluginChatCommand("/killcube");
        GetNode<Button>("%RespawnButton").Pressed += () => NetworkClient.Instance.SendPluginChatCommand("/respawncube");

        // #307: a real, unrestricted teleport — evil-cube-plugin's
        // `teleport-entity` host-function call skips the server's normal
        // movement validation entirely (speed cap, collision, navmesh), so
        // this actually goes anywhere, unlike Move/WASD.
        var xEdit = GetNode<LineEdit>("%XEdit");
        UiHelpers.LockMovementWhileFocused(xEdit);
        var yEdit = GetNode<LineEdit>("%YEdit");
        UiHelpers.LockMovementWhileFocused(yEdit);
        var zEdit = GetNode<LineEdit>("%ZEdit");
        UiHelpers.LockMovementWhileFocused(zEdit);
        GetNode<Button>("%TeleportButton").Pressed += () =>
            NetworkClient.Instance.SendPluginChatCommand($"/teleport {xEdit.Text.Trim()} {yEdit.Text.Trim()} {zEdit.Text.Trim()}");
    }
}
