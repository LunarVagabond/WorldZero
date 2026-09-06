using Godot;
using WorldZeroTestGrounds.Net;
using WorldZeroTestGrounds.State;

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
// backend even if this panel were somehow shown to them.
public partial class AdminPanel : Control
{
    public override void _Ready()
    {
        SetAnchorsPreset(LayoutPreset.FullRect);
        var box = UiHelpers.CreateScrollableColumn(this);

        UiHelpers.AddWrappingLabel(box, "Every command here is re-checked server-side against your account's real \"admin\" or \"qa\" role — this panel only ever shows for an account that already announced it has one of them.");

        BuildGrantSection(box);
        BuildCurrencySection(box);
        BuildCubeSection(box);
        BuildTeleportSection(box);
    }

    private static void BuildGrantSection(Control parent)
    {
        var section = UiHelpers.Section(parent, "Grant item (/grant)");
        var itemEdit = new LineEdit { PlaceholderText = "item_type (e.g. iron-ore)" };
        UiHelpers.LockMovementWhileFocused(itemEdit);
        section.AddChild(itemEdit);
        var qtyEdit = new LineEdit { PlaceholderText = "quantity", Text = "1" };
        UiHelpers.LockMovementWhileFocused(qtyEdit);
        section.AddChild(qtyEdit);
        var button = new Button { Text = "Grant to self" };
        button.Pressed += () => NetworkClient.Instance.SendPluginChatCommand($"/grant {itemEdit.Text.Trim()} {qtyEdit.Text.Trim()}");
        section.AddChild(button);
    }

    private static void BuildCurrencySection(Control parent)
    {
        var section = UiHelpers.Section(parent, "Grant currency (/grantcurrency)");
        var keyEdit = new LineEdit { PlaceholderText = "currency_key (e.g. gold)" };
        UiHelpers.LockMovementWhileFocused(keyEdit);
        section.AddChild(keyEdit);
        var amountEdit = new LineEdit { PlaceholderText = "amount", Text = "100" };
        UiHelpers.LockMovementWhileFocused(amountEdit);
        section.AddChild(amountEdit);
        var button = new Button { Text = "Grant to self" };
        button.Pressed += () => NetworkClient.Instance.SendPluginChatCommand($"/grantcurrency {keyEdit.Text.Trim()} {amountEdit.Text.Trim()}");
        section.AddChild(button);
    }

    private static void BuildCubeSection(Control parent)
    {
        var section = UiHelpers.Section(parent, "Evil Cube (/killcube, /respawncube)");
        var row = new HBoxContainer();
        section.AddChild(row);
        var killButton = new Button { Text = "Kill cube" };
        killButton.Pressed += () => NetworkClient.Instance.SendPluginChatCommand("/killcube");
        row.AddChild(killButton);
        var respawnButton = new Button { Text = "Respawn cube" };
        respawnButton.Pressed += () => NetworkClient.Instance.SendPluginChatCommand("/respawncube");
        row.AddChild(respawnButton);
    }

    // #307: a real, unrestricted teleport — evil-cube-plugin's
    // `teleport-entity` host-function call skips the server's normal
    // movement validation entirely (speed cap, collision, navmesh), so
    // this actually goes anywhere, unlike Move/WASD.
    private static void BuildTeleportSection(Control parent)
    {
        var section = UiHelpers.Section(parent, "Teleport (/teleport, admin/QA only)");
        var row = new HBoxContainer();
        section.AddChild(row);
        var xEdit = new LineEdit { PlaceholderText = "x", CustomMinimumSize = new Vector2(60, 0) };
        UiHelpers.LockMovementWhileFocused(xEdit);
        row.AddChild(xEdit);
        var yEdit = new LineEdit { PlaceholderText = "y", CustomMinimumSize = new Vector2(60, 0) };
        UiHelpers.LockMovementWhileFocused(yEdit);
        row.AddChild(yEdit);
        var zEdit = new LineEdit { PlaceholderText = "z", Text = "0", CustomMinimumSize = new Vector2(60, 0) };
        UiHelpers.LockMovementWhileFocused(zEdit);
        row.AddChild(zEdit);
        var button = new Button { Text = "Teleport self" };
        button.Pressed += () => NetworkClient.Instance.SendPluginChatCommand($"/teleport {xEdit.Text.Trim()} {yEdit.Text.Trim()} {zEdit.Text.Trim()}");
        section.AddChild(button);
    }
}
