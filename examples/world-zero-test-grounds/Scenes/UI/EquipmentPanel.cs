using System.Linq;
using Godot;
using WorldZeroTestGrounds.Net;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes.UI;

// Equip/unequip (#307, session.proto's EquipItem/UnequipItem/
// EquipmentChanged). No wire message exposes equipment.schema.yaml's
// slot mapping — a client has no way to know in advance which of its
// items are equippable or which slot they'd occupy. This panel is
// deliberately "try it and see": type an item_type/slot, send the
// request, and the server's real Error reply (surfaced via the event
// log, same as every other rejected action in this project) is the only
// available feedback for "that's not equippable" or "wrong slot name."
public partial class EquipmentPanel : Control
{
    private Label _equippedLabel = null!;

    public override void _Ready()
    {
        SetAnchorsPreset(LayoutPreset.FullRect);
        var box = UiHelpers.CreateScrollableColumn(this);

        UiHelpers.AddWrappingLabel(box,
            "There's no wire message exposing which item_types are equippable or what slot they use — this just sends the request and shows the server's real Error if it's rejected.")
            .Modulate = AppTheme.Warning;

        var equipSection = UiHelpers.Section(box, "Equip");
        var equipRow = new HBoxContainer();
        equipSection.AddChild(equipRow);
        var itemEdit = new LineEdit { PlaceholderText = "item_type", SizeFlagsHorizontal = SizeFlags.ExpandFill };
        UiHelpers.LockMovementWhileFocused(itemEdit);
        equipRow.AddChild(itemEdit);
        var equipButton = new Button { Text = "Equip" };
        equipButton.Pressed += () => NetworkClient.Instance.SendEquipItem(itemEdit.Text.Trim());
        equipRow.AddChild(equipButton);

        var unequipSection = UiHelpers.Section(box, "Unequip");
        var unequipRow = new HBoxContainer();
        unequipSection.AddChild(unequipRow);
        var slotEdit = new LineEdit { PlaceholderText = "slot", SizeFlagsHorizontal = SizeFlags.ExpandFill };
        UiHelpers.LockMovementWhileFocused(slotEdit);
        unequipRow.AddChild(slotEdit);
        var unequipButton = new Button { Text = "Unequip" };
        unequipButton.Pressed += () => NetworkClient.Instance.SendUnequipItem(slotEdit.Text.Trim());
        unequipRow.AddChild(unequipButton);

        box.AddChild(new HSeparator());
        UiHelpers.AddWrappingLabel(box, "Currently equipped:");
        _equippedLabel = UiHelpers.AddWrappingLabel(box, "(nothing)");

        var nc = NetworkClient.Instance;
        nc.OnEquipmentChanged += _ => Refresh();
        nc.OnJoined += _ => Refresh();
        Refresh();
    }

    private void Refresh()
    {
        var equipped = GameState.Instance.EquippedItems;
        _equippedLabel.Text = equipped.Count == 0
            ? "(nothing)"
            : string.Join("\n", equipped.Select(kv => $"{kv.Key}: {kv.Value}"));
    }
}
