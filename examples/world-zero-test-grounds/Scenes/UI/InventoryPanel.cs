using Godot;
using WorldZeroTestGrounds.Net;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes.UI;

// Real inventory display + UseItem exerciser (session.proto's ItemChanged/
// UseItem) — the only two inventory-adjacent actions the backend actually
// supports today. `GameState.OwnItems` is a flat item_type -> quantity
// dictionary because that's genuinely all the backend has:
// docs/specs/Data_Model_Spec.md is explicit that character items are "not
// slot-based... no notion of inventory slots, equipment positions, or item
// instances with individual properties" (tied to #112's out-of-scope
// call). So there's deliberately no "move items around" here — there's
// nothing to move between, just a per-type quantity. Dropping an item into
// the world and player-to-player trading are both entirely unbuilt
// server-side (no wire message, no handler, not even a stub) — see
// BACKEND_INTEGRATION_NOTES.md. Equipment is unbuilt too, not just
// missing a UI: there is no equip/unequip concept, no slot types, and no
// stat effect from "wearing" anything anywhere in the backend.
public partial class InventoryPanel : Control
{
    private VBoxContainer _rows = null!;
    private Label _emptyLabel = null!;

    public override void _Ready()
    {
        SetAnchorsPreset(LayoutPreset.FullRect);
        var box = UiHelpers.CreateScrollableColumn(this);

        UiHelpers.AddWrappingLabel(box,
            "Real backend actions only: quantities push live via ItemChanged; \"Use\" sends a real UseItem. There is no slot/order concept to rearrange (items are a flat item_type -> quantity count, not slot-based), and drop/trade/equip are all unbuilt server-side, not just missing UI here — see BACKEND_INTEGRATION_NOTES.md.")
            .Modulate = new Color(0.9f, 0.85f, 0.4f);

        _emptyLabel = UiHelpers.AddWrappingLabel(box, "(no items — grant some via the Admin tab, or Craft)");
        _rows = new VBoxContainer { SizeFlagsHorizontal = SizeFlags.ExpandFill };
        box.AddChild(_rows);

        var nc = NetworkClient.Instance;
        nc.OnItemChanged += _ => Refresh();
        nc.OnJoined += _ => Refresh();
        Refresh();
    }

    public void Refresh()
    {
        foreach (Node child in _rows.GetChildren())
        {
            child.QueueFree();
        }

        var items = GameState.Instance.OwnItems;
        int shown = 0;
        foreach (var (itemType, quantity) in items)
        {
            // ItemChanged's "resulting value, not delta" convention means
            // a fully-consumed stack still leaves a 0 entry behind in the
            // dictionary — filter those out of display rather than
            // removing them from GameState, in case a later push resumes
            // counting up from a real prior value.
            if (quantity <= 0)
            {
                continue;
            }
            shown++;
            var row = new HBoxContainer();
            row.AddChild(new Label { Text = $"{itemType}  x{quantity}", SizeFlagsHorizontal = SizeFlags.ExpandFill });
            var useButton = new Button { Text = "Use" };
            useButton.Pressed += () => NetworkClient.Instance.SendUseItem(itemType);
            row.AddChild(useButton);
            _rows.AddChild(row);
        }
        _emptyLabel.Visible = shown == 0;
    }
}
