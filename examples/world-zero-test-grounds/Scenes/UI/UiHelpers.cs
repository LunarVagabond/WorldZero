using Godot;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes.UI;

public static class UiHelpers
{
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
