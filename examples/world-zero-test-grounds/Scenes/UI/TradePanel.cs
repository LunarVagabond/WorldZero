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
// than trying to track confirmation locally. Layout lives in TradePanel.tscn.
public partial class TradePanel : Control
{
    private LineEdit _targetEdit = null!;
    private Label _incomingRequestLabel = null!;
    private Label _stateLabel = null!;

    public override void _Ready()
    {
        _targetEdit = GetNode<LineEdit>("%TargetEdit");
        _incomingRequestLabel = GetNode<Label>("%IncomingRequestLabel");
        _stateLabel = GetNode<Label>("%StateLabel");

        UiHelpers.LockMovementWhileFocused(_targetEdit);
        GetNode<Button>("%UseTargetButton").Pressed += () => _targetEdit.Text = GameState.Instance.CurrentTargetEntityId ?? "";
        GetNode<Button>("%RequestButton").Pressed += () => NetworkClient.Instance.SendTradeRequest(_targetEdit.Text.Trim());

        GetNode<Button>("%AcceptButton").Pressed += () => NetworkClient.Instance.SendTradeRequestResponse(true);
        GetNode<Button>("%DeclineButton").Pressed += () => NetworkClient.Instance.SendTradeRequestResponse(false);

        var itemEdit = GetNode<LineEdit>("%ItemEdit");
        UiHelpers.LockMovementWhileFocused(itemEdit);
        var itemQtyEdit = GetNode<LineEdit>("%ItemQtyEdit");
        UiHelpers.LockMovementWhileFocused(itemQtyEdit);
        GetNode<Button>("%OfferItemButton").Pressed += () =>
        {
            if (long.TryParse(itemQtyEdit.Text.Trim(), out var qty))
            {
                NetworkClient.Instance.SendTradeOfferItem(itemEdit.Text.Trim(), qty);
            }
        };

        var currencyEdit = GetNode<LineEdit>("%CurrencyEdit");
        UiHelpers.LockMovementWhileFocused(currencyEdit);
        var currencyAmountEdit = GetNode<LineEdit>("%CurrencyAmountEdit");
        UiHelpers.LockMovementWhileFocused(currencyAmountEdit);
        GetNode<Button>("%OfferCurrencyButton").Pressed += () =>
        {
            if (long.TryParse(currencyAmountEdit.Text.Trim(), out var amount))
            {
                NetworkClient.Instance.SendTradeOfferCurrency(currencyEdit.Text.Trim(), amount);
            }
        };

        GetNode<Button>("%ConfirmButton").Pressed += () => NetworkClient.Instance.SendTradeConfirm();
        GetNode<Button>("%CancelButton").Pressed += () => NetworkClient.Instance.SendTradeCancel();

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
