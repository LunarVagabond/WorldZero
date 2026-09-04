using Godot;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes.UI;

// Shared layout helper — every panel in this project was overflowing its
// visible area (long forms + the event log had no scrolling, so content
// past the bottom of the window was just gone). Every panel now builds
// its content inside a scrollable column instead of a bare VBoxContainer
// directly under the panel root.
public static class UiHelpers
{
    public static VBoxContainer CreateScrollableColumn(Control parent)
    {
        var scroll = new ScrollContainer
        {
            // A single long unwrapped Label (e.g. DebugOverlay's
            // concatenated id/position lines) can force this column
            // wider than the dock, which made ScrollContainer offer
            // horizontal scroll instead of vertical — disabling
            // horizontal scrolling outright forces long content to wrap
            // (see AddWrappingLabel below) rather than ever scrolling
            // sideways.
            HorizontalScrollMode = ScrollContainer.ScrollMode.Disabled,
        };
        scroll.SetAnchorsPreset(Control.LayoutPreset.FullRect);
        parent.AddChild(scroll);

        var box = new VBoxContainer { SizeFlagsHorizontal = Control.SizeFlags.ExpandFill };
        scroll.AddChild(box);
        return box;
    }

    // A plain `new Label { Text = ... }` doesn't wrap and will happily
    // force its parent (and this scrollable column) wider than the
    // dock — use this instead for any label whose text isn't a short,
    // known-fixed string.
    public static Label AddWrappingLabel(Control parent, string text = "")
    {
        var label = new Label
        {
            Text = text,
            AutowrapMode = TextServer.AutowrapMode.WordSmart,
            SizeFlagsHorizontal = Control.SizeFlags.ExpandFill,
        };
        parent.AddChild(label);
        return label;
    }

    // A titled, visually bordered group — used to break the login and
    // character-select forms into clearly separated, easy-to-scan
    // sections instead of one long unbroken column of fields.
    public static VBoxContainer Section(Control parent, string title)
    {
        var panel = new PanelContainer();
        parent.AddChild(panel);

        var box = new VBoxContainer();
        panel.AddChild(box);

        if (!string.IsNullOrEmpty(title))
        {
            var header = new Label { Text = title, Modulate = new Color(0.75f, 0.85f, 1f) };
            header.AddThemeFontSizeOverride("font_size", 16);
            box.AddChild(header);
            box.AddChild(new HSeparator());
        }

        return box;
    }

    // Wire onto every LineEdit that lives in the in-world HUD tabs
    // (Chat/Party/Guild/Craft/Admin) — WorldController's WASD handling
    // polls `Input.IsKeyPressed` directly, which bypasses normal Godot
    // focus/input-consumption, so without this a chat message like
    // "sad" also moves the player south-west-down while being typed.
    // FocusEntered/FocusExited toggle GameState.TextInputActive (checked
    // once per frame in WorldController); Enter (submit) or Escape both
    // release focus so movement resumes immediately rather than waiting
    // for a click elsewhere.
    public static void LockMovementWhileFocused(LineEdit edit)
    {
        edit.FocusEntered += () => GameState.Instance.TextInputActive = true;
        edit.FocusExited += () => GameState.Instance.TextInputActive = false;
        edit.TextSubmitted += _ => edit.ReleaseFocus();
        edit.GuiInput += @event =>
        {
            if (@event is InputEventKey { Pressed: true, Keycode: Key.Escape })
            {
                edit.ReleaseFocus();
            }
        };
    }
}
