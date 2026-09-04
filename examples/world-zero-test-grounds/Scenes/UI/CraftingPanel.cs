using Godot;
using WorldZeroTestGrounds.Net;

namespace WorldZeroTestGrounds.Scenes.UI;

// CraftItem{recipe_key} (PROMPT.md §13, §18 step 15a) — no recipe-browsing
// query exists, so this is deliberately just "type a known recipe_key
// and watch ItemChanged land in the debug console," per the doc's own
// framing.
public partial class CraftingPanel : Control
{
    public override void _Ready()
    {
        SetAnchorsPreset(LayoutPreset.FullRect);
        var box = UiHelpers.CreateScrollableColumn(this);

        UiHelpers.AddWrappingLabel(box, "recipe_key (must be declared in crafting.schema.yaml on the backend):");
        var row = new HBoxContainer();
        box.AddChild(row);
        var recipeEdit = new LineEdit { SizeFlagsHorizontal = SizeFlags.ExpandFill };
        UiHelpers.LockMovementWhileFocused(recipeEdit);
        row.AddChild(recipeEdit);
        var craftButton = new Button { Text = "Craft" };
        craftButton.Pressed += () => NetworkClient.Instance.SendCraftItem(recipeEdit.Text.Trim());
        row.AddChild(craftButton);

        UiHelpers.AddWrappingLabel(box, "No success/failure reply beyond ItemChanged pushes (or Error) — watch the Debug tab's event log.");
    }
}
