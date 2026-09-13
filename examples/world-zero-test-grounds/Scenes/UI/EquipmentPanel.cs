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
// Layout lives in EquipmentPanel.tscn.
public partial class EquipmentPanel : Control
{
    private Label _equippedLabel = null!;

    public override void _Ready()
    {
        _equippedLabel = GetNode<Label>("%EquippedLabel");

        var itemEdit = GetNode<LineEdit>("%ItemEdit");
        UiHelpers.LockMovementWhileFocused(itemEdit);
        GetNode<Button>("%EquipButton").Pressed += () => NetworkClient.Instance.SendEquipItem(itemEdit.Text.Trim());

        var slotEdit = GetNode<LineEdit>("%SlotEdit");
        UiHelpers.LockMovementWhileFocused(slotEdit);
        GetNode<Button>("%UnequipButton").Pressed += () => NetworkClient.Instance.SendUnequipItem(slotEdit.Text.Trim());

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
