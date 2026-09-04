using Godot;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes.UI;

// The bottom dock — full width, not full-height-on-the-right (the
// original layout overwhelmed the screen with whatever panel was
// active). A single-selection tab strip (Debug/Chat/Party/Guild/Craft/
// Inventory, plus Admin once your account announces the `admin` role):
// only one subsystem's contents are ever visible at a time, and the
// active tab's output gets the whole dock width rather than splitting
// it with every other panel side by side (the previous
// CollapsiblePanel-row layout).
public partial class Hud : Control
{
    private TabContainer _tabs = null!;
    private Control _inventoryTab = null!;
    private Control? _adminTab;

    public override void _Ready()
    {
        // Full-width strip pinned to the bottom edge, sized relative to
        // the viewport rather than a hardcoded size.
        AnchorLeft = 0f;
        AnchorRight = 1f;
        AnchorTop = 1f;
        AnchorBottom = 1f;
        OffsetLeft = 8;
        OffsetRight = -8;
        OffsetBottom = -8;
        OffsetTop = -320;

        _tabs = new TabContainer();
        _tabs.SetAnchorsPreset(LayoutPreset.FullRect);
        AddChild(_tabs);

        AddTab("Debug", new DebugOverlay());
        AddTab("Chat", new ChatPanel());
        AddTab("Party", new PartyPanel());
        AddTab("Guild", new GuildPanel());
        AddTab("Craft", new CraftingPanel());
        _inventoryTab = new InventoryPanel();
        AddTab("Inventory", _inventoryTab);

        GameState.Instance.RolesChanged += OnRolesChanged;
        OnRolesChanged();
    }

    // 'I' jumps straight to the Inventory tab — checked here (rather
    // than in WorldController) since Hud owns the TabContainer. Stays
    // in the tree with Visible=false outside InWorld (Main.cs toggles
    // visibility, not tree membership), so this still gets called then;
    // guard on ConnectionState instead of Visible. Also guarded on
    // TextInputActive so typing a literal "i" into chat/any HUD text
    // field doesn't yank focus into a different tab out from under it.
    public override void _UnhandledInput(InputEvent @event)
    {
        if (@event is InputEventKey { Pressed: true, Keycode: Key.I }
            && GameState.Instance.ConnectionState == ConnectionState.InWorld
            && !GameState.Instance.TextInputActive)
        {
            _tabs.CurrentTab = _tabs.GetTabIdxFromControl(_inventoryTab);
            GetViewport().SetInputAsHandled();
        }
    }

    private void AddTab(string title, Control content)
    {
        content.Name = title;
        content.SizeFlagsHorizontal = SizeFlags.ExpandFill;
        content.SizeFlagsVertical = SizeFlags.ExpandFill;
        _tabs.AddChild(content);
    }

    private void OnRolesChanged()
    {
        bool shouldShow = GameState.Instance.IsAdmin;
        bool alreadyShown = _adminTab is not null;
        if (shouldShow == alreadyShown)
        {
            return;
        }

        if (shouldShow)
        {
            _adminTab = new AdminPanel();
            AddTab("Admin", _adminTab);
        }
        else if (_adminTab is not null)
        {
            _adminTab.QueueFree();
            _adminTab = null;
        }
    }
}
