using System;
using Godot;
using Google.Protobuf;
using WorldZeroTestGrounds.State;
using WAuth = WorldZeroTestGrounds.Wire.Auth;
using WRealm = WorldZeroTestGrounds.Wire.Realm;
using WChar = WorldZeroTestGrounds.Wire.Character;
using WChat = WorldZeroTestGrounds.Wire.Chat;
using WSession = WorldZeroTestGrounds.Wire.Session;

namespace WorldZeroTestGrounds.Net;

// The autoload that owns the socket, decodes every incoming envelope by
// message_type (PROMPT.md §2.3) into its typed ServerMessage, and fans
// it out as a C# event. Scenes/UI never touch GameConnection or
// Envelope directly — they call the Send* helpers here and subscribe to
// the On* events. Every decoded envelope is also mirrored into
// GameState's event log (§16's required debug console) regardless of
// whether anything else here handles it.
public partial class NetworkClient : Node
{
    public static NetworkClient Instance { get; private set; } = null!;

    private GameConnection? _connection;
    private uint _nextMoveSeq = 1;

    public bool IsSocketConnected => _connection?.IsConnected ?? false;

    // --- Auth (message_type 1) ---
    public event Action<WAuth.Authenticated>? OnAuthenticated;
    public event Action<string>? OnAuthError;

    // --- Realm (message_type 2) ---
    public event Action<WRealm.RealmList>? OnRealmList;
    public event Action<WRealm.RealmSelected>? OnRealmSelected;
    public event Action<string>? OnRealmError;

    // --- Character (message_type 3) ---
    public event Action<WChar.CharacterList>? OnCharacterList;
    public event Action<WChar.CharacterCreated>? OnCharacterCreated;
    public event Action<WChar.CharacterSelected>? OnCharacterSelected;
    public event Action<WChar.CharacterOptions>? OnCharacterOptions;
    public event Action<string>? OnCharacterError;

    // --- Chat (message_type 100) ---
    public event Action<WChat.Joined>? OnChatJoined;
    public event Action<WChat.Left>? OnChatLeft;
    public event Action<WChat.Chat>? OnChatMessage;
    public event Action<string>? OnChatError;

    // --- Session / world (message_type 200) ---
    public event Action<WSession.Joined>? OnJoined;
    public event Action<WSession.EntitySpawned>? OnEntitySpawned;
    public event Action<WSession.EntityDespawned>? OnEntityDespawned;
    public event Action<WSession.ZoneChanged>? OnZoneChanged;
    public event Action<WSession.Moved>? OnMoved;
    public event Action<WSession.Rejected>? OnRejected;
    public event Action<string>? OnSessionError;
    public event Action<WSession.PluginMessage>? OnPluginMessage;
    public event Action<WSession.Pong>? OnPong;
    public event Action<WSession.PartyInviteReceived>? OnPartyInviteReceived;
    public event Action<WSession.PartyInviteDeclined>? OnPartyInviteDeclined;
    public event Action<WSession.PartyUpdate>? OnPartyUpdate;
    public event Action<WSession.GuildInviteReceived>? OnGuildInviteReceived;
    public event Action<WSession.GuildInviteDeclined>? OnGuildInviteDeclined;
    public event Action<WSession.GuildUpdate>? OnGuildUpdate;
    public event Action? OnGuildDisbanded;
    public event Action<WSession.StatChanged>? OnStatChanged;
    public event Action<WSession.ItemChanged>? OnItemChanged;
    public event Action<WSession.CurrencyChanged>? OnCurrencyChanged;

    public event Action<string>? OnDisconnected;

    public override void _EnterTree()
    {
        Instance = this;
    }

    public async System.Threading.Tasks.Task<bool> ConnectAsync(string host, int port)
    {
        _connection = new GameConnection();
        try
        {
            await _connection.ConnectAsync(host, port);
            GameState.Instance.SetConnectionState(ConnectionState.Connected);
            GameState.Instance.LogEvent("net", $"connected to {host}:{port}");
            return true;
        }
        catch (Exception ex)
        {
            GameState.Instance.LogEvent("net", $"connect failed: {ex.Message}");
            GameState.Instance.SetConnectionState(ConnectionState.Disconnected);
            OnDisconnected?.Invoke(ex.Message);
            return false;
        }
    }

    public void Disconnect()
    {
        _connection?.Disconnect();
        GameState.Instance.SetConnectionState(ConnectionState.Disconnected);
    }

    public override void _Process(double delta)
    {
        if (_connection is null)
        {
            return;
        }

        while (_connection.DisconnectReasons.TryDequeue(out var reason))
        {
            GameState.Instance.LogEvent("net", reason);
            GameState.Instance.SetConnectionState(ConnectionState.Disconnected);
            OnDisconnected?.Invoke(reason);
        }

        while (_connection.Incoming.TryDequeue(out var frame))
        {
            Dispatch(frame.MessageType, frame.Payload);
        }
    }

    private void Dispatch(ushort messageType, byte[] payload)
    {
        try
        {
            switch (messageType)
            {
                case MessageType.Auth:
                    DispatchAuth(WAuth.ServerMessage.Parser.ParseFrom(payload));
                    break;
                case MessageType.Realm:
                    DispatchRealm(WRealm.ServerMessage.Parser.ParseFrom(payload));
                    break;
                case MessageType.Character:
                    DispatchCharacter(WChar.ServerMessage.Parser.ParseFrom(payload));
                    break;
                case MessageType.Chat:
                    DispatchChat(WChat.ServerMessage.Parser.ParseFrom(payload));
                    break;
                case MessageType.Session:
                    DispatchSession(WSession.ServerMessage.Parser.ParseFrom(payload));
                    break;
                default:
                    GameState.Instance.LogEvent("net", $"unhandled message_type {messageType} ({payload.Length} bytes)");
                    break;
            }
        }
        catch (Exception ex)
        {
            GameState.Instance.LogEvent("net", $"failed to decode message_type {messageType}: {ex.Message}");
        }
    }

    // --- Auth dispatch ---

    private void DispatchAuth(WAuth.ServerMessage msg)
    {
        switch (msg.KindCase)
        {
            case WAuth.ServerMessage.KindOneofCase.Authenticated:
                GameState.Instance.LogEvent("auth", $"Authenticated account={msg.Authenticated.AccountId} username={msg.Authenticated.Username}");
                OnAuthenticated?.Invoke(msg.Authenticated);
                break;
            case WAuth.ServerMessage.KindOneofCase.Error:
                GameState.Instance.LogEvent("auth", $"Error: {msg.Error.Message}");
                OnAuthError?.Invoke(msg.Error.Message);
                break;
        }
    }

    public void SendRegister(string username, string password)
    {
        var m = new WAuth.ClientMessage { Register = new WAuth.Register { Username = username, Password = password } };
        _connection!.Send(MessageType.Auth, m.ToByteArray());
        GameState.Instance.LogEvent("auth", $"-> Register {username}");
    }

    public void SendLogin(string username, string password)
    {
        var m = new WAuth.ClientMessage { Login = new WAuth.Login { Username = username, Password = password } };
        _connection!.Send(MessageType.Auth, m.ToByteArray());
        GameState.Instance.LogEvent("auth", $"-> Login {username}");
    }

    public void SendResume(string sessionToken)
    {
        var m = new WAuth.ClientMessage { Resume = new WAuth.Resume { SessionToken = sessionToken } };
        _connection!.Send(MessageType.Auth, m.ToByteArray());
        GameState.Instance.LogEvent("auth", "-> Resume");
    }

    // --- Realm dispatch ---

    private void DispatchRealm(WRealm.ServerMessage msg)
    {
        switch (msg.KindCase)
        {
            case WRealm.ServerMessage.KindOneofCase.RealmList:
                GameState.Instance.LogEvent("realm", $"RealmList ({msg.RealmList.Realms.Count} realms)");
                OnRealmList?.Invoke(msg.RealmList);
                break;
            case WRealm.ServerMessage.KindOneofCase.RealmSelected:
                GameState.Instance.LogEvent("realm", $"RealmSelected {msg.RealmSelected.RealmId}");
                OnRealmSelected?.Invoke(msg.RealmSelected);
                break;
            case WRealm.ServerMessage.KindOneofCase.Error:
                GameState.Instance.LogEvent("realm", $"Error: {msg.Error.Message}");
                OnRealmError?.Invoke(msg.Error.Message);
                break;
        }
    }

    public void SendListRealms()
    {
        var m = new WRealm.ClientMessage { ListRealms = new WRealm.ListRealms() };
        _connection!.Send(MessageType.Realm, m.ToByteArray());
        GameState.Instance.LogEvent("realm", "-> ListRealms");
    }

    public void SendSelectRealm(string realmId)
    {
        var m = new WRealm.ClientMessage { SelectRealm = new WRealm.SelectRealm { RealmId = realmId } };
        _connection!.Send(MessageType.Realm, m.ToByteArray());
        GameState.Instance.LogEvent("realm", $"-> SelectRealm {realmId}");
    }

    // --- Character dispatch ---

    private void DispatchCharacter(WChar.ServerMessage msg)
    {
        switch (msg.KindCase)
        {
            case WChar.ServerMessage.KindOneofCase.CharacterList:
                GameState.Instance.LogEvent("character", $"CharacterList ({msg.CharacterList.Characters.Count} characters)");
                OnCharacterList?.Invoke(msg.CharacterList);
                break;
            case WChar.ServerMessage.KindOneofCase.CharacterCreated:
                GameState.Instance.LogEvent("character", $"CharacterCreated {msg.CharacterCreated.CharacterId}");
                OnCharacterCreated?.Invoke(msg.CharacterCreated);
                break;
            case WChar.ServerMessage.KindOneofCase.CharacterSelected:
                GameState.Instance.LogEvent("character", $"CharacterSelected {msg.CharacterSelected.CharacterId}");
                OnCharacterSelected?.Invoke(msg.CharacterSelected);
                break;
            case WChar.ServerMessage.KindOneofCase.CharacterOptions:
                GameState.Instance.LogEvent("character", $"CharacterOptions ({msg.CharacterOptions.Archetypes.Count} archetypes)");
                OnCharacterOptions?.Invoke(msg.CharacterOptions);
                break;
            case WChar.ServerMessage.KindOneofCase.Error:
                GameState.Instance.LogEvent("character", $"Error: {msg.Error.Message}");
                OnCharacterError?.Invoke(msg.Error.Message);
                break;
        }
    }

    public void SendListCharacters()
    {
        var m = new WChar.ClientMessage { ListCharacters = new WChar.ListCharacters() };
        _connection!.Send(MessageType.Character, m.ToByteArray());
        GameState.Instance.LogEvent("character", "-> ListCharacters");
    }

    public void SendListCharacterOptions()
    {
        var m = new WChar.ClientMessage { ListCharacterOptions = new WChar.ListCharacterOptions() };
        _connection!.Send(MessageType.Character, m.ToByteArray());
        GameState.Instance.LogEvent("character", "-> ListCharacterOptions");
    }

    public void SendCreateCharacter(string name, string archetypeKey = "")
    {
        var m = new WChar.ClientMessage { CreateCharacter = new WChar.CreateCharacter { Name = name, ArchetypeKey = archetypeKey } };
        _connection!.Send(MessageType.Character, m.ToByteArray());
        GameState.Instance.LogEvent("character", $"-> CreateCharacter {name} archetype={archetypeKey}");
    }

    public void SendSelectCharacter(string characterId)
    {
        var m = new WChar.ClientMessage { SelectCharacter = new WChar.SelectCharacter { CharacterId = characterId } };
        _connection!.Send(MessageType.Character, m.ToByteArray());
        GameState.Instance.LogEvent("character", $"-> SelectCharacter {characterId}");
    }

    // --- Chat dispatch ---

    private void DispatchChat(WChat.ServerMessage msg)
    {
        switch (msg.KindCase)
        {
            case WChat.ServerMessage.KindOneofCase.Joined:
                GameState.Instance.LogEvent("chat", $"Joined {msg.Joined.Channel} ({msg.Joined.ChannelId})");
                GameState.Instance.ChatChannels[msg.Joined.Channel] = msg.Joined.ChannelId;
                OnChatJoined?.Invoke(msg.Joined);
                break;
            case WChat.ServerMessage.KindOneofCase.Left:
                GameState.Instance.LogEvent("chat", $"Left {msg.Left.Channel}");
                GameState.Instance.ChatChannels.Remove(msg.Left.Channel);
                OnChatLeft?.Invoke(msg.Left);
                break;
            case WChat.ServerMessage.KindOneofCase.Chat:
                GameState.Instance.LogEvent("chat", $"[{msg.Chat.Channel}] {msg.Chat.Sender}: {msg.Chat.Body}");
                OnChatMessage?.Invoke(msg.Chat);
                break;
            case WChat.ServerMessage.KindOneofCase.Error:
                GameState.Instance.LogEvent("chat", $"Error: {msg.Error.Message}");
                OnChatError?.Invoke(msg.Error.Message);
                break;
        }
    }

    public void SendChatJoin(string channel)
    {
        var m = new WChat.ClientMessage { Join = new WChat.Join { Channel = channel } };
        _connection!.Send(MessageType.Chat, m.ToByteArray());
        GameState.Instance.LogEvent("chat", $"-> Join {channel}");
    }

    public void SendChatLeave(string channel)
    {
        var m = new WChat.ClientMessage { Leave = new WChat.Leave { Channel = channel } };
        _connection!.Send(MessageType.Chat, m.ToByteArray());
        GameState.Instance.LogEvent("chat", $"-> Leave {channel}");
    }

    public void SendChatSend(string channelId, string body)
    {
        var m = new WChat.ClientMessage { Send = new WChat.Send { ChannelId = channelId, Body = body } };
        _connection!.Send(MessageType.Chat, m.ToByteArray());
        GameState.Instance.LogEvent("chat", $"-> Send [{channelId}] {body}");
    }

    // A plugin-declared chat command (e.g. "/killcube") is intercepted
    // and dispatched by `server::session` before `channel_id` is ever
    // resolved against a real joined channel — but the wire-level parse
    // of `Send.channel_id` into a UUID still happens unconditionally
    // first (see BACKEND_INTEGRATION_NOTES.md), so a syntactically valid
    // placeholder is required even though the value is never used for a
    // matched command. Centralized here so callers (the Admin panel)
    // don't need to know about this rough edge.
    private const string PluginCommandPlaceholderChannelId = "00000000-0000-0000-0000-000000000000";

    public void SendPluginChatCommand(string commandWithArgs) => SendChatSend(PluginCommandPlaceholderChannelId, commandWithArgs);

    // --- Session dispatch ---

    private void DispatchSession(WSession.ServerMessage msg)
    {
        switch (msg.KindCase)
        {
            case WSession.ServerMessage.KindOneofCase.Joined:
                GameState.Instance.LogEvent("session", $"Joined entity={msg.Joined.EntityId} at ({msg.Joined.X:F1},{msg.Joined.Y:F1}) roster={msg.Joined.Roster.Count} tick={msg.Joined.Tick}");
                OnJoined?.Invoke(msg.Joined);
                break;
            case WSession.ServerMessage.KindOneofCase.EntitySpawned:
                GameState.Instance.LogEvent("session", $"EntitySpawned {msg.EntitySpawned.EntityType} {msg.EntitySpawned.EntityId} at ({msg.EntitySpawned.X:F1},{msg.EntitySpawned.Y:F1})");
                OnEntitySpawned?.Invoke(msg.EntitySpawned);
                break;
            case WSession.ServerMessage.KindOneofCase.EntityDespawned:
                GameState.Instance.LogEvent("session", $"EntityDespawned {msg.EntityDespawned.EntityId}");
                OnEntityDespawned?.Invoke(msg.EntityDespawned);
                break;
            case WSession.ServerMessage.KindOneofCase.ZoneChanged:
                GameState.Instance.LogEvent("session", $"ZoneChanged -> {msg.ZoneChanged.ZoneId} at ({msg.ZoneChanged.X:F1},{msg.ZoneChanged.Y:F1}) roster={msg.ZoneChanged.Roster.Count} tick={msg.ZoneChanged.Tick}");
                OnZoneChanged?.Invoke(msg.ZoneChanged);
                break;
            case WSession.ServerMessage.KindOneofCase.Moved:
                GameState.Instance.LogEvent("session", $"Moved {msg.Moved.EntityId} -> ({msg.Moved.X:F1},{msg.Moved.Y:F1}) seq={msg.Moved.Seq} tick={msg.Moved.Tick}", quiet: true);
                OnMoved?.Invoke(msg.Moved);
                break;
            case WSession.ServerMessage.KindOneofCase.Rejected:
                GameState.Instance.LogEvent("session", $"Rejected seq={msg.Rejected.Seq} tick={msg.Rejected.Tick} reason={msg.Rejected.Reason}");
                OnRejected?.Invoke(msg.Rejected);
                break;
            case WSession.ServerMessage.KindOneofCase.Error:
                GameState.Instance.LogEvent("session", $"Error: {msg.Error.Message}");
                OnSessionError?.Invoke(msg.Error.Message);
                break;
            case WSession.ServerMessage.KindOneofCase.PluginMessage:
                GameState.Instance.LogEvent("session", $"PluginMessage: {msg.PluginMessage.Body}");
                // evil-cube-plugin's ad-hoc "roles:role1,role2" convention
                // (announced on zone join) — the only way this client
                // learns its own account roles, since core has no wire
                // message for it (see BACKEND_INTEGRATION_NOTES.md).
                if (msg.PluginMessage.Body.StartsWith("roles:"))
                {
                    var roles = msg.PluginMessage.Body["roles:".Length..]
                        .Split(',', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
                    GameState.Instance.SetRoles(roles);
                }
                OnPluginMessage?.Invoke(msg.PluginMessage);
                break;
            case WSession.ServerMessage.KindOneofCase.Pong:
                OnPong?.Invoke(msg.Pong);
                break;
            case WSession.ServerMessage.KindOneofCase.PartyInviteReceived:
                GameState.Instance.LogEvent("party", $"PartyInviteReceived from={msg.PartyInviteReceived.FromEntityId}");
                OnPartyInviteReceived?.Invoke(msg.PartyInviteReceived);
                break;
            case WSession.ServerMessage.KindOneofCase.PartyInviteDeclined:
                GameState.Instance.LogEvent("party", $"PartyInviteDeclined by={msg.PartyInviteDeclined.ByEntityId}");
                OnPartyInviteDeclined?.Invoke(msg.PartyInviteDeclined);
                break;
            case WSession.ServerMessage.KindOneofCase.PartyUpdate:
                GameState.Instance.LogEvent("party", $"PartyUpdate members=[{string.Join(",", msg.PartyUpdate.Members)}]");
                OnPartyUpdate?.Invoke(msg.PartyUpdate);
                break;
            case WSession.ServerMessage.KindOneofCase.GuildInviteReceived:
                GameState.Instance.LogEvent("guild", $"GuildInviteReceived from={msg.GuildInviteReceived.FromEntityId}");
                OnGuildInviteReceived?.Invoke(msg.GuildInviteReceived);
                break;
            case WSession.ServerMessage.KindOneofCase.GuildInviteDeclined:
                GameState.Instance.LogEvent("guild", $"GuildInviteDeclined by={msg.GuildInviteDeclined.ByEntityId}");
                OnGuildInviteDeclined?.Invoke(msg.GuildInviteDeclined);
                break;
            case WSession.ServerMessage.KindOneofCase.GuildUpdate:
                GameState.Instance.LogEvent("guild", $"GuildUpdate {msg.GuildUpdate.Name} members={msg.GuildUpdate.Members.Count}");
                OnGuildUpdate?.Invoke(msg.GuildUpdate);
                break;
            case WSession.ServerMessage.KindOneofCase.GuildDisbanded:
                GameState.Instance.LogEvent("guild", "GuildDisbanded");
                OnGuildDisbanded?.Invoke();
                break;
            case WSession.ServerMessage.KindOneofCase.StatChanged:
                GameState.Instance.LogEvent("stats", $"StatChanged {msg.StatChanged.StatKey}={msg.StatChanged.Value}");
                GameState.Instance.OwnStats[msg.StatChanged.StatKey] = msg.StatChanged.Value;
                OnStatChanged?.Invoke(msg.StatChanged);
                break;
            case WSession.ServerMessage.KindOneofCase.ItemChanged:
                GameState.Instance.LogEvent("items", $"ItemChanged {msg.ItemChanged.ItemType}={msg.ItemChanged.Quantity}");
                GameState.Instance.OwnItems[msg.ItemChanged.ItemType] = msg.ItemChanged.Quantity;
                OnItemChanged?.Invoke(msg.ItemChanged);
                break;
            case WSession.ServerMessage.KindOneofCase.CurrencyChanged:
                GameState.Instance.LogEvent("currency", $"CurrencyChanged {msg.CurrencyChanged.CurrencyKey}={msg.CurrencyChanged.Balance}");
                GameState.Instance.OwnCurrency[msg.CurrencyChanged.CurrencyKey] = msg.CurrencyChanged.Balance;
                OnCurrencyChanged?.Invoke(msg.CurrencyChanged);
                break;
        }
    }

    public void SendMove(double x, double y, out uint seq)
    {
        seq = _nextMoveSeq++;
        var m = new WSession.ClientMessage { Move = new WSession.Move { X = x, Y = y, Seq = seq } };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("session", $"-> Move ({x:F1},{y:F1}) seq={seq}", quiet: true);
    }

    public void SendAttack(string targetEntityId, string statKey)
    {
        var m = new WSession.ClientMessage { Attack = new WSession.Attack { TargetEntityId = targetEntityId, StatKey = statKey } };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("session", $"-> Attack {targetEntityId} stat={statKey}");
    }

    public void SendUseItem(string itemType)
    {
        var m = new WSession.ClientMessage { UseItem = new WSession.UseItem { ItemType = itemType } };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("session", $"-> UseItem {itemType}");
    }

    public void SendInteractNpc(string npcEntityId)
    {
        var m = new WSession.ClientMessage { InteractNpc = new WSession.InteractNpc { NpcEntityId = npcEntityId } };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("session", $"-> InteractNpc {npcEntityId}");
    }

    public void SendPing(long clientSentAtMillis)
    {
        var m = new WSession.ClientMessage { Ping = new WSession.Ping { ClientSentAt = clientSentAtMillis } };
        _connection!.Send(MessageType.Session, m.ToByteArray());
    }

    public void SendPartyInvite(string targetEntityId, string partyType = "")
    {
        var m = new WSession.ClientMessage { PartyInvite = new WSession.PartyInvite { TargetEntityId = targetEntityId, PartyType = partyType } };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("party", $"-> PartyInvite {targetEntityId}");
    }

    public void SendPartyInviteResponse(bool accept)
    {
        var m = new WSession.ClientMessage { PartyInviteResponse = new WSession.PartyInviteResponse { Accept = accept } };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("party", $"-> PartyInviteResponse accept={accept}");
    }

    public void SendPartyLeave()
    {
        var m = new WSession.ClientMessage { PartyLeave = new WSession.PartyLeave() };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("party", "-> PartyLeave");
    }

    public void SendGuildCreate(string name)
    {
        var m = new WSession.ClientMessage { GuildCreate = new WSession.GuildCreate { Name = name } };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("guild", $"-> GuildCreate {name}");
    }

    public void SendGuildInvite(string targetEntityId)
    {
        var m = new WSession.ClientMessage { GuildInvite = new WSession.GuildInvite { TargetEntityId = targetEntityId } };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("guild", $"-> GuildInvite {targetEntityId}");
    }

    public void SendGuildInviteResponse(bool accept)
    {
        var m = new WSession.ClientMessage { GuildInviteResponse = new WSession.GuildInviteResponse { Accept = accept } };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("guild", $"-> GuildInviteResponse accept={accept}");
    }

    public void SendGuildLeave()
    {
        var m = new WSession.ClientMessage { GuildLeave = new WSession.GuildLeave() };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("guild", "-> GuildLeave");
    }

    public void SendGuildDisband()
    {
        var m = new WSession.ClientMessage { GuildDisband = new WSession.GuildDisband() };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("guild", "-> GuildDisband");
    }

    public void SendGuildKick(string targetEntityId)
    {
        var m = new WSession.ClientMessage { GuildKick = new WSession.GuildKick { TargetEntityId = targetEntityId } };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("guild", $"-> GuildKick {targetEntityId}");
    }

    public void SendGuildPromote(string targetEntityId, string rankKey)
    {
        var m = new WSession.ClientMessage { GuildPromote = new WSession.GuildPromote { TargetEntityId = targetEntityId, RankKey = rankKey } };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("guild", $"-> GuildPromote {targetEntityId} -> {rankKey}");
    }

    public void SendGuildDemote(string targetEntityId, string rankKey)
    {
        var m = new WSession.ClientMessage { GuildDemote = new WSession.GuildDemote { TargetEntityId = targetEntityId, RankKey = rankKey } };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("guild", $"-> GuildDemote {targetEntityId} -> {rankKey}");
    }

    public void SendGuildSetMotd(string motd)
    {
        var m = new WSession.ClientMessage { GuildSetMotd = new WSession.GuildSetMotd { Motd = motd } };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("guild", $"-> GuildSetMotd {motd}");
    }

    public void SendGuildSetTag(string tag)
    {
        var m = new WSession.ClientMessage { GuildSetTag = new WSession.GuildSetTag { Tag = tag } };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("guild", $"-> GuildSetTag {tag}");
    }

    public void SendCraftItem(string recipeKey)
    {
        var m = new WSession.ClientMessage { CraftItem = new WSession.CraftItem { RecipeKey = recipeKey } };
        _connection!.Send(MessageType.Session, m.ToByteArray());
        GameState.Instance.LogEvent("crafting", $"-> CraftItem {recipeKey}");
    }
}
