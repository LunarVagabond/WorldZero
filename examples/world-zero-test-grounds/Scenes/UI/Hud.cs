using Godot;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes.UI;

// The bottom dock — full width, not full-height-on-the-right (the
// original layout overwhelmed the screen with whatever panel was
// active). A single-selection tab strip (Debug/Chat/Party/Guild/Craft/
// Inventory/Equipment/Trade, plus Admin once your account announces the
// `admin` or `qa` role — #307's lighter-weight test-account role):
// only one subsystem's contents are ever visible at a time, and the
// active tab's output gets the whole dock width rather than splitting
// it with every other panel side by side (the previous
// CollapsiblePanel-row layout). Static tabs are instanced in Hud.tscn;
// the Admin tab stays dynamic here since its presence depends on the
// account's role.
public partial class Hud : Control
{
    private static readonly PackedScene AdminPanelScene = GD.Load<PackedScene>("res://Scenes/UI/AdminPanel.tscn");

    private TabContainer _tabs = null!;
    private Control _inventoryTab = null!;
    private Control? _adminTab;

    public override void _Ready()
    {
        _tabs = GetNode<TabContainer>("%Tabs");
        _inventoryTab = GetNode<Control>("%Inventory");

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

    private void OnRolesChanged()
    {
        // #307: `qa` is a lighter-weight way to grant a test account the
        // same tooling `admin` gets, without using the real admin
        // designation — same tab either way, gated on either role.
        bool shouldShow = GameState.Instance.IsAdmin || GameState.Instance.IsQa;
        bool alreadyShown = _adminTab is not null;
        if (shouldShow == alreadyShown)
        {
            return;
        }

        if (shouldShow)
        {
            _adminTab = (Control)AdminPanelScene.Instantiate();
            _adminTab.Name = "Admin";
            _adminTab.SizeFlagsHorizontal = SizeFlags.ExpandFill;
            _adminTab.SizeFlagsVertical = SizeFlags.ExpandFill;
            _tabs.AddChild(_adminTab);
        }
        else if (_adminTab is not null)
        {
            _adminTab.QueueFree();
            _adminTab = null;
        }
    }
}
