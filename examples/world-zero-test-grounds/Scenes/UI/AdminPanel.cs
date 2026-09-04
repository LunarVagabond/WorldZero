using Godot;
using WorldZeroTestGrounds.Net;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes.UI;

// Admin-only commands, backed by evil-cube-plugin's real caller-role-gated
// chat commands (docs/specs/Auth_Spec.md's "Account roles" — the one
// real, backend-enforced privilege mechanism World Zero has; there is no
// core wire concept of "admin" beyond it). This panel's own visibility
// (Hud.cs only adds it once GameState.IsAdmin says so) is purely a UI
// convenience — every command below still gets independently re-checked
// server-side, so a non-admin account is blocked by the actual backend
// even if this panel were somehow shown to them.
public partial class AdminPanel : Control
{
    public override void _Ready()
    {
        SetAnchorsPreset(LayoutPreset.FullRect);
        var box = UiHelpers.CreateScrollableColumn(this);

        UiHelpers.AddWrappingLabel(box, "Every command here is re-checked server-side against your account's real \"admin\" role — this panel only ever shows for an account that already announced it has one.");

        BuildGrantSection(box);
        BuildCurrencySection(box);
        BuildCubeSection(box);
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
}
