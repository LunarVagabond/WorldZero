using Godot;

namespace WorldZeroTestGrounds.Scenes.UI;

// The one place this project's visual identity is defined (#307) — panel
// layout lives in each panel's own .tscn, so a single `Theme` applied to
// every top-level panel and inherited by its children is the only
// practical way to make the whole app read as one system instead of
// Godot's unstyled default gray. This file only ever adds visual
// styling, never layout logic.
public static class AppTheme
{
    // The three colors already in ad hoc use across Login/CharacterSelect/
    // Inventory/RealmSelect/UiHelpers before this file existed — formalized
    // here as the actual palette instead of six separate inline `Color(...)`
    // literals that happened to agree with each other by copy-paste.
    public static readonly Color Accent = new(0.75f, 0.85f, 1f);
    public static readonly Color Warning = new(0.9f, 0.85f, 0.4f);
    public static readonly Color Error = new(1f, 0.35f, 0.35f);
    public static readonly Color Success = new(0.45f, 0.85f, 0.5f);

    private static readonly Color Background = new(0.11f, 0.12f, 0.15f);
    private static readonly Color PanelBackground = new(0.16f, 0.17f, 0.21f);
    private static readonly Color BodyText = new(0.88f, 0.89f, 0.92f);
    private static readonly Color QuietText = new(0.6f, 0.62f, 0.68f);
    private static readonly Color ButtonNormal = new(0.22f, 0.24f, 0.3f);
    private static readonly Color ButtonHover = new(0.28f, 0.31f, 0.4f);
    private static readonly Color ButtonPressed = new(0.35f, 0.55f, 0.85f);
    private static readonly Color FieldBackground = new(0.09f, 0.1f, 0.12f);
    private static readonly Color Border = new(0.26f, 0.28f, 0.34f);

    // `CanvasLayer` (what `Main.cs` parents every top-level panel under)
    // isn't itself a `Control`, so there's no single root to set `.Theme`
    // on once — `Main.cs` assigns this to each top-level panel
    // individually instead. One shared instance either way, built once.
    public static readonly Theme Instance = Build();

    public static Theme Build()
    {
        var theme = new Theme();

        theme.SetColor("font_color", "Label", BodyText);
        theme.SetFontSize("font_size", "Label", 14);

        theme.SetStylebox("panel", "PanelContainer", FlatPanel(PanelBackground, Border));
        theme.SetStylebox("panel", "ScrollContainer", FlatPanel(Background, Border, borderWidth: 0));

        theme.SetStylebox("normal", "Button", FlatButton(ButtonNormal));
        theme.SetStylebox("hover", "Button", FlatButton(ButtonHover));
        theme.SetStylebox("pressed", "Button", FlatButton(ButtonPressed));
        theme.SetStylebox("disabled", "Button", FlatButton(ButtonNormal, alpha: 0.5f));
        theme.SetStylebox("focus", "Button", FlatButton(ButtonHover, Accent));
        theme.SetColor("font_color", "Button", BodyText);
        theme.SetColor("font_hover_color", "Button", BodyText);
        theme.SetColor("font_pressed_color", "Button", Color.Color8(20, 20, 25));
        theme.SetColor("font_disabled_color", "Button", QuietText);
        theme.SetFontSize("font_size", "Button", 14);

        var fieldStyle = FlatPanel(FieldBackground, Border);
        theme.SetStylebox("normal", "LineEdit", fieldStyle);
        theme.SetStylebox("focus", "LineEdit", FlatPanel(FieldBackground, Accent));
        theme.SetColor("font_color", "LineEdit", BodyText);
        theme.SetColor("font_placeholder_color", "LineEdit", QuietText);
        theme.SetFontSize("font_size", "LineEdit", 14);

        theme.SetStylebox("tab_selected", "TabContainer", FlatPanel(PanelBackground, Accent, borderWidth: 2));
        theme.SetStylebox("tab_unselected", "TabContainer", FlatPanel(Background, Border));
        theme.SetStylebox("tab_disabled", "TabContainer", FlatPanel(Background, Border, alpha: 0.5f));
        theme.SetStylebox("panel", "TabContainer", FlatPanel(PanelBackground, Border));
        theme.SetColor("font_selected_color", "TabContainer", Accent);
        theme.SetColor("font_unselected_color", "TabContainer", QuietText);

        theme.SetStylebox("separator", "HSeparator", new StyleBoxLine { Color = Border, Thickness = 1 });

        return theme;
    }

    private static StyleBoxFlat FlatPanel(Color background, Color border, int borderWidth = 1, float alpha = 1f)
    {
        var color = background;
        color.A *= alpha;
        return new StyleBoxFlat
        {
            BgColor = color,
            BorderColor = border,
            BorderWidthLeft = borderWidth,
            BorderWidthRight = borderWidth,
            BorderWidthTop = borderWidth,
            BorderWidthBottom = borderWidth,
            CornerRadiusTopLeft = 4,
            CornerRadiusTopRight = 4,
            CornerRadiusBottomLeft = 4,
            CornerRadiusBottomRight = 4,
            ContentMarginLeft = 8,
            ContentMarginRight = 8,
            ContentMarginTop = 6,
            ContentMarginBottom = 6,
        };
    }

    private static StyleBoxFlat FlatButton(Color background, Color? border = null, float alpha = 1f)
    {
        var style = FlatPanel(background, border ?? Border, borderWidth: 1, alpha: alpha);
        style.ContentMarginLeft = 12;
        style.ContentMarginRight = 12;
        style.ContentMarginTop = 6;
        style.ContentMarginBottom = 6;
        return style;
    }
}
