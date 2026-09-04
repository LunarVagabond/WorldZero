using Godot;
using WorldZeroTestGrounds.Net;
using WorldZeroTestGrounds.Wire.Realm;

namespace WorldZeroTestGrounds.Scenes.UI;

// ListRealms/SelectRealm (PROMPT.md §2.5, §3.1) — the WoW-style "pick a
// world" screen between login and character select. A `server` process
// today only ever serves exactly one realm (#130 is the unbuilt
// multi-realm-per-process feature), so this list will usually show a
// single row — but it's still real, live data (name/open-or-bound/
// character_count/live_connection_count straight off `RealmList`), not
// a hardcoded picker, so the flow is already correct for whenever #130
// lands. Same Build*Section pattern as LoginPanel/CharacterSelectPanel.
//
// `character_count` is a realm-wide population census, not "characters
// I can select here" — for an `open` realm (OSRS-style: one character
// pool shared across the whole open-realm group) those differ, since
// `ListCharacters` after selecting spans every open realm's characters
// for this account. `selectable_character_count` (world_zero#261) is
// the number that actually matches what Character Select will show, for
// both realm types, so that's what this panel displays.
// Snapshot-only (a Refresh button, no polling): `online` is the only
// number here that changes on its own moment to moment, and re-fetching
// every few seconds just to watch a number that's usually static isn't
// worth the traffic for a manual test tool.
public partial class RealmSelectPanel : Control
{
    private ItemList _realmList = null!;
    private Label _statusLabel = null!;
    private RealmList? _lastList;

    public override void _Ready()
    {
        SetAnchorsPreset(LayoutPreset.FullRect);

        var panel = new PanelContainer();
        panel.SetAnchorsPreset(LayoutPreset.Center);
        AddChild(panel);

        var root = new VBoxContainer { CustomMinimumSize = new Vector2(420, 0) };
        panel.AddChild(root);

        root.AddChild(new Label { Text = "Select a realm", HorizontalAlignment = HorizontalAlignment.Center });
        BuildStatusLabel(root);
        BuildRealmListSection(root);
        BuildUtilitySection(root);

        var nc = NetworkClient.Instance;
        nc.OnRealmList += HandleRealmList;
        nc.OnRealmError += HandleRealmError;
    }

    private void BuildStatusLabel(Control parent)
    {
        _statusLabel = UiHelpers.AddWrappingLabel(parent);
        _statusLabel.Modulate = new Color(1f, 0.35f, 0.35f);
    }

    private void BuildRealmListSection(Control parent)
    {
        var section = UiHelpers.Section(parent, "Available realms");
        _realmList = new ItemList { CustomMinimumSize = new Vector2(0, 160) };
        section.AddChild(_realmList);
        _realmList.ItemActivated += index => SelectIndex((int)index);

        var selectButton = new Button { Text = "Select highlighted" };
        selectButton.Pressed += () => SelectIndex(_realmList.GetSelectedItems() is { Length: > 0 } sel ? sel[0] : -1);
        section.AddChild(selectButton);
    }

    private void BuildUtilitySection(Control parent)
    {
        var section = UiHelpers.Section(parent, "");
        var refreshButton = new Button { Text = "Refresh list" };
        refreshButton.Pressed += RefreshOnShow;
        section.AddChild(refreshButton);
    }

    private void HandleRealmError(string msg)
    {
        _statusLabel.Text = $"Error: {msg}";
    }

    private void HandleRealmList(RealmList list)
    {
        _lastList = list;
        _realmList.Clear();
        foreach (var r in list.Realms)
        {
            string label = $"{r.Name}  [{r.OpenOrBound}]  characters={r.SelectableCharacterCount}  online={r.LiveConnectionCount}  ({r.RealmId})";
            _realmList.AddItem(label);
        }
        if (_realmList.ItemCount > 0)
        {
            _realmList.Select(0);
        }
    }

    private void SelectIndex(int index)
    {
        if (_lastList is null || index < 0 || index >= _lastList.Realms.Count)
        {
            _statusLabel.Text = "Highlight a realm first.";
            return;
        }
        NetworkClient.Instance.SendSelectRealm(_lastList.Realms[index].RealmId);
    }

    public void RefreshOnShow()
    {
        NetworkClient.Instance.SendListRealms();
    }
}
