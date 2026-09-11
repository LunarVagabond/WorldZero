using Godot;
using WorldZeroTestGrounds.Net;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes.UI;

// Real inventory display + actions (#307) — this file's own doc comment
// used to say drop/trade/equip were entirely unbuilt server-side; that
// was true when this panel was first written but is stale now (#276/#277/
// #278 landed real DropItem/EquipItem/TradeRequest wire messages and
// server-side handling since). Equipment gets its own EquipmentPanel;
// trade gets its own TradePanel; this panel keeps Use/Drop/MoveToSlot,
// since those are all "act on this exact stack in my inventory" actions.
// Static frame lives in InventoryPanel.tscn; the item rows are still
// built here since their count varies at runtime with the account's
// actual inventory.
public partial class InventoryPanel : Control
{
    private VBoxContainer _rows = null!;
    private Label _emptyLabel = null!;

    public override void _Ready()
    {
        _rows = GetNode<VBoxContainer>("%Rows");
        _emptyLabel = GetNode<Label>("%EmptyLabel");

        var nc = NetworkClient.Instance;
        nc.OnItemChanged += _ => Refresh();
        nc.OnItemMoved += _ => Refresh();
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
            _rows.AddChild(BuildItemRow(itemType, quantity));
        }
        _emptyLabel.Visible = shown == 0;
    }

    private static HBoxContainer BuildItemRow(string itemType, long quantity)
    {
        var row = new HBoxContainer();
        row.AddChild(new Label { Text = $"{itemType}  x{quantity}", SizeFlagsHorizontal = SizeFlags.ExpandFill });

        var useButton = new Button { Text = "Use" };
        useButton.Pressed += () => NetworkClient.Instance.SendUseItem(itemType);
        row.AddChild(useButton);

        var dropQtyEdit = new LineEdit { PlaceholderText = "qty", Text = "1", CustomMinimumSize = new Vector2(50, 0) };
        UiHelpers.LockMovementWhileFocused(dropQtyEdit);
        row.AddChild(dropQtyEdit);
        var dropButton = new Button { Text = "Drop" };
        dropButton.Pressed += () =>
        {
            if (long.TryParse(dropQtyEdit.Text.Trim(), out var qty) && qty > 0)
            {
                NetworkClient.Instance.SendDropItem(itemType, qty);
            }
        };
        row.AddChild(dropButton);

        var slotEdit = new LineEdit { PlaceholderText = "slot #", CustomMinimumSize = new Vector2(60, 0) };
        UiHelpers.LockMovementWhileFocused(slotEdit);
        row.AddChild(slotEdit);
        var moveButton = new Button { Text = "Move to slot" };
        moveButton.Pressed += () =>
        {
            if (int.TryParse(slotEdit.Text.Trim(), out var slot) && slot >= 0)
            {
                NetworkClient.Instance.SendMoveItemToSlot(itemType, slot);
            }
        };
        row.AddChild(moveButton);

        return row;
    }
}
