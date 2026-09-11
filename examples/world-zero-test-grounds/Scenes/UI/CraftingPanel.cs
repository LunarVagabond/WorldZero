using Godot;
using WorldZeroTestGrounds.Net;

namespace WorldZeroTestGrounds.Scenes.UI;

// CraftItem{recipe_key} (PROMPT.md §13, §18 step 15a) — no recipe-browsing
// query exists, so this is deliberately just "type a known recipe_key
// and watch ItemChanged land in the debug console," per the doc's own
// framing. Layout lives in CraftingPanel.tscn.
public partial class CraftingPanel : Control
{
    public override void _Ready()
    {
        var recipeEdit = GetNode<LineEdit>("%RecipeEdit");
        UiHelpers.LockMovementWhileFocused(recipeEdit);
        GetNode<Button>("%CraftButton").Pressed += () => NetworkClient.Instance.SendCraftItem(recipeEdit.Text.Trim());
    }
}
