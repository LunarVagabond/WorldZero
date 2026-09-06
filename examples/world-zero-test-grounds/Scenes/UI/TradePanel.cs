using Godot;
using WorldZeroTestGrounds.Net;
using WorldZeroTestGrounds.State;

namespace WorldZeroTestGrounds.Scenes.UI;

// Real player-to-player trade (#307, session.proto's TradeRequest/
// TradeOfferItem/TradeOfferCurrency/TradeConfirm/TradeCancel). Target
// just needs to be connected (any zone) — no same-zone requirement, unlike
// party's JoinGroupLayer. Any offer change resets both sides' confirmed
// flag server-side (the anti-scam mechanism), so this panel always
// re-renders the whole state from the latest TradeStateChanged rather
// than trying to track confirmation locally.
public partial class TradePanel : Control
{
    private LineEdit _targetEdit = null!;
    private Label _incomingRequestLabel = null!;
    private Label _stateLabel = null!;

    public override void _Ready()
    {
        SetAnchorsPreset(LayoutPreset.FullRect);
        var box = UiHelpers.CreateScrollableColumn(this);

        var requestSection = UiHelpers.Section(box, "Request a trade");
        var targetRow = new HBoxContainer();
        requestSection.AddChild(targetRow);
        _targetEdit = new LineEdit { PlaceholderText = "target entity id", SizeFlagsHorizontal = SizeFlags.ExpandFill };
        UiHelpers.LockMovementWhileFocused(_targetEdit);
        targetRow.AddChild(_targetEdit);
        var useTargetButton = new Button { Text = "Use current target" };
        useTargetButton.Pressed += () => _targetEdit.Text = GameState.Instance.CurrentTargetEntityId ?? "";
        targetRow.AddChild(useTargetButton);
        var requestButton = new Button { Text = "Request trade" };
        requestButton.Pressed += () => NetworkClient.Instance.SendTradeRequest(_targetEdit.Text.Trim());
        requestSection.AddChild(requestButton);

        var incomingSection = UiHelpers.Section(box, "Incoming request");
        _incomingRequestLabel = UiHelpers.AddWrappingLabel(incomingSection, "(none)");
        var incomingRow = new HBoxContainer();
        incomingSection.AddChild(incomingRow);
        var acceptButton = new Button { Text = "Accept" };
        acceptButton.Pressed += () => NetworkClient.Instance.SendTradeRequestResponse(true);
        incomingRow.AddChild(acceptButton);
        var declineButton = new Button { Text = "Decline" };
        declineButton.Pressed += () => NetworkClient.Instance.SendTradeRequestResponse(false);
        incomingRow.AddChild(declineButton);

        var offerSection = UiHelpers.Section(box, "Your offer");
        var itemRow = new HBoxContainer();
        offerSection.AddChild(itemRow);
        var itemEdit = new LineEdit { PlaceholderText = "item_type", SizeFlagsHorizontal = SizeFlags.ExpandFill };
        UiHelpers.LockMovementWhileFocused(itemEdit);
        itemRow.AddChild(itemEdit);
        var itemQtyEdit = new LineEdit { PlaceholderText = "qty (0 removes)", Text = "1", CustomMinimumSize = new Vector2(90, 0) };
        UiHelpers.LockMovementWhileFocused(itemQtyEdit);
        itemRow.AddChild(itemQtyEdit);
        var offerItemButton = new Button { Text = "Offer item" };
        offerItemButton.Pressed += () =>
        {
            if (long.TryParse(itemQtyEdit.Text.Trim(), out var qty))
            {
                NetworkClient.Instance.SendTradeOfferItem(itemEdit.Text.Trim(), qty);
            }
        };
        itemRow.AddChild(offerItemButton);

        var currencyRow = new HBoxContainer();
        offerSection.AddChild(currencyRow);
        var currencyEdit = new LineEdit { PlaceholderText = "currency_key", SizeFlagsHorizontal = SizeFlags.ExpandFill };
        UiHelpers.LockMovementWhileFocused(currencyEdit);
        currencyRow.AddChild(currencyEdit);
        var currencyAmountEdit = new LineEdit { PlaceholderText = "amount (0 removes)", Text = "1", CustomMinimumSize = new Vector2(90, 0) };
        UiHelpers.LockMovementWhileFocused(currencyAmountEdit);
        currencyRow.AddChild(currencyAmountEdit);
        var offerCurrencyButton = new Button { Text = "Offer currency" };
        offerCurrencyButton.Pressed += () =>
        {
            if (long.TryParse(currencyAmountEdit.Text.Trim(), out var amount))
            {
                NetworkClient.Instance.SendTradeOfferCurrency(currencyEdit.Text.Trim(), amount);
            }
        };
        currencyRow.AddChild(offerCurrencyButton);

        var confirmRow = new HBoxContainer();
        offerSection.AddChild(confirmRow);
        var confirmButton = new Button { Text = "Confirm" };
        confirmButton.Pressed += () => NetworkClient.Instance.SendTradeConfirm();
        confirmRow.AddChild(confirmButton);
        var cancelButton = new Button { Text = "Cancel trade" };
        cancelButton.Pressed += () => NetworkClient.Instance.SendTradeCancel();
        confirmRow.AddChild(cancelButton);

        box.AddChild(new HSeparator());
        UiHelpers.AddWrappingLabel(box, "Active trade state:");
        _stateLabel = UiHelpers.AddWrappingLabel(box, "(no active trade)");

        var nc = NetworkClient.Instance;
        nc.OnTradeRequestReceived += msg => _incomingRequestLabel.Text = $"From {msg.FromEntityId}";
        nc.OnTradeRequestDeclined += msg => GameState.Instance.LogEvent("trade", $"Your request was declined by {msg.ByEntityId}");
        nc.OnTradeStateChanged += _ =>
        {
            _incomingRequestLabel.Text = "(none)";
            Refresh();
        };
        nc.OnTradeCancelled += _ => Refresh();
        nc.OnTradeCompleted += Refresh;
        Refresh();
    }

    private void Refresh()
    {
        var trade = GameState.Instance.ActiveTrade;
        if (trade is null)
        {
            _stateLabel.Text = "(no active trade)";
            return;
        }

        string FormatOffer(System.Collections.Generic.IReadOnlyList<(string ItemType, long Quantity)> items, System.Collections.Generic.IReadOnlyList<(string CurrencyKey, long Amount)> currency)
        {
            var parts = new System.Collections.Generic.List<string>();
            foreach (var (itemType, quantity) in items)
            {
                parts.Add($"{itemType} x{quantity}");
            }
            foreach (var (currencyKey, amount) in currency)
            {
                parts.Add($"{currencyKey} x{amount}");
            }
            return parts.Count == 0 ? "(empty)" : string.Join(", ", parts);
        }

        _stateLabel.Text =
            $"With: {trade.OtherEntityId}\n" +
            $"Your offer ({(trade.YourConfirmed ? "confirmed" : "not confirmed")}): {FormatOffer(trade.YourItems, trade.YourCurrency)}\n" +
            $"Their offer ({(trade.TheirConfirmed ? "confirmed" : "not confirmed")}): {FormatOffer(trade.TheirItems, trade.TheirCurrency)}";
    }
}
