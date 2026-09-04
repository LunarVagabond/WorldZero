using System;
using System.Collections.Generic;
using Godot;
using WorldZeroTestGrounds.Movement;
using WorldZeroTestGrounds.Net;
using WorldZeroTestGrounds.State;
using WSession = WorldZeroTestGrounds.Wire.Session;

namespace WorldZeroTestGrounds.Scenes;

// The 3D "barely-dressed debug tool" scene (PROMPT.md §18's closing
// note) — primitive geometry only. Server (x, y) maps to this scene's
// (X, Z) ground plane; Y is purely a client-side visual convenience the
// server has no opinion about (§5.1's 2D-only limitation).
public partial class WorldController : Node3D
{
    // Known zone footprints from PROMPT.md §5.2's shipped content —
    // used only to size the debug floor plane; the server never sends
    // bounds, so an unrecognized zone_id falls back to a generic size.
    private static readonly Dictionary<string, (float Width, float Depth)> ZoneFootprints = new()
    {
        ["greenwood-forest"] = (500f, 500f),
        ["stonebridge-village"] = (200f, 200f),
    };

    private const float MoveSpeedMps = 6.0f; // under the server's 10 m/s cap (§6.2)
    private const float MoveSendIntervalSec = 0.05f; // ~20Hz, matching the world tick rate (§6.2)
    private const float PlayerHeight = 1.8f;
    private const float EntityCollisionRadius = 0.5f;

    // Joined (session.proto) carries no zone_id field at all — only
    // ZoneChanged does. There is genuinely no way for the client to know
    // which zone a freshly-selected character spawned into until either
    // it crosses a link live, or we just assume the server's own
    // configured default (§9.2's WZ_CONFIG_DIR content-pack.yaml lists
    // greenwood-forest first). Right almost always for a test-grounds
    // character; if wrong, only the cosmetic floor size is off until the
    // first real ZoneChanged corrects it.
    private const string DefaultInitialZoneId = "greenwood-forest";

    private Camera3D _camera = null!;
    private MeshInstance3D _floor = null!;
    private Node3D _selfVisual = null!;
    private Node3D _entitiesRoot = null!;
    private Node3D? _gridLines;
    private Node3D? _pylons;
    private Node3D? _originMarker;

    private readonly Dictionary<string, VisualEntity> _visuals = new();
    private float _sendAccumulator;
    private float _pingAccumulator;
    private Vector2 _lastSentPos = new(float.NaN, float.NaN);

    private sealed class VisualEntity
    {
        public required Node3D Node;
        public required string EntityType;
        public readonly EntityInterpolator Interpolator = new();
        public Label3D? HealthLabel;
    }

    public override void _Ready()
    {
        SetupEnvironment();

        var nc = NetworkClient.Instance;
        nc.OnJoined += HandleJoined;
        nc.OnZoneChanged += HandleZoneChanged;
        nc.OnEntitySpawned += HandleEntitySpawned;
        nc.OnEntityDespawned += HandleEntityDespawned;
        nc.OnMoved += HandleMoved;
        nc.OnRejected += HandleRejected;
        nc.OnPluginMessage += HandlePluginMessage;
        nc.OnPong += HandlePong;
    }

    private void SetupEnvironment()
    {
        var light = new DirectionalLight3D
        {
            RotationDegrees = new Vector3(-55, -35, 0),
            LightEnergy = 1.1f,
        };
        AddChild(light);

        var env = new Godot.Environment
        {
            BackgroundMode = Godot.Environment.BGMode.Sky,
            Sky = new Sky { SkyMaterial = new ProceduralSkyMaterial() },
            AmbientLightSource = Godot.Environment.AmbientSource.Sky,
        };
        var worldEnv = new WorldEnvironment { Environment = env };
        AddChild(worldEnv);

        _floor = new MeshInstance3D
        {
            Mesh = new PlaneMesh { Size = new Vector2(300, 300) },
            MaterialOverride = new StandardMaterial3D { AlbedoColor = new Color(0.25f, 0.32f, 0.22f) },
        };
        AddChild(_floor);

        _entitiesRoot = new Node3D { Name = "Entities" };
        AddChild(_entitiesRoot);

        _selfVisual = BuildCapsule(new Color(0.2f, 0.6f, 1f));
        _selfVisual.Name = "Self";
        AddChild(_selfVisual);

        _camera = new Camera3D
        {
            Position = new Vector3(0, 9, 10),
        };
        AddChild(_camera);
        _camera.LookAt(new Vector3(0, 0, 0), Vector3.Up);
    }

    private static Node3D BuildCapsule(Color color)
    {
        var root = new Node3D();
        var mesh = new MeshInstance3D
        {
            Mesh = new CapsuleMesh { Radius = 0.4f, Height = PlayerHeight },
            Position = new Vector3(0, PlayerHeight / 2f, 0),
            MaterialOverride = new StandardMaterial3D { AlbedoColor = color },
        };
        root.AddChild(mesh);
        return root;
    }

    private static Node3D BuildBox(Color color, float size = 1.5f)
    {
        var root = new Node3D();
        var mesh = new MeshInstance3D
        {
            Mesh = new BoxMesh { Size = new Vector3(size, size, size) },
            Position = new Vector3(0, size / 2f, 0),
            MaterialOverride = new StandardMaterial3D { AlbedoColor = color },
        };
        root.AddChild(mesh);
        return root;
    }

    private static Node3D BuildPickable(Node3D visual, float radius)
    {
        var area = new Area3D { Name = "Pickable" };
        var shape = new CollisionShape3D { Shape = new SphereShape3D { Radius = radius } };
        shape.Position = new Vector3(0, radius, 0);
        area.AddChild(shape);
        visual.AddChild(area);
        return area;
    }

    private void SizeFloorForZone(string zoneId)
    {
        var (w, d) = ZoneFootprints.GetValueOrDefault(zoneId, (300f, 300f));
        ((PlaneMesh)_floor.Mesh).Size = new Vector2(w, d);
        // Server (0,0) is a corner, not a center (PROMPT.md §5.1 zone
        // manifests) — offset the floor so world (0,0,0) sits at that
        // same corner instead of the plane's own center.
        _floor.Position = new Vector3(w / 2f, 0, d / 2f);
        BuildGroundReferences(w, d);
    }

    // A single flat-colored plane gives no visual cue that you're
    // actually moving — reported after the first real playtest ("can't
    // see anything in the form of movement due to camera angle and the
    // fact that there is no reference objects"). Adds a ground grid
    // (MultiMesh of thin scaled boxes) plus sparse pylons for parallax,
    // and a marker at server (0,0) — all primitive geometry, matching
    // this project's "barely-dressed debug tool" brief.
    private void BuildGroundReferences(float width, float depth)
    {
        _gridLines?.QueueFree();
        _pylons?.QueueFree();
        _originMarker?.QueueFree();

        const float step = 20f;
        const float thickness = 0.2f;
        const float yThickness = 0.06f;
        int vCount = Mathf.FloorToInt(width / step) + 1;
        int hCount = Mathf.FloorToInt(depth / step) + 1;

        var lineMultiMesh = new MultiMesh
        {
            TransformFormat = MultiMesh.TransformFormatEnum.Transform3D,
            Mesh = new BoxMesh { Size = Vector3.One },
            InstanceCount = vCount + hCount,
        };
        int idx = 0;
        for (int i = 0; i < vCount; i++)
        {
            float x = i * step;
            lineMultiMesh.SetInstanceTransform(idx++, new Transform3D(
                Basis.Identity.Scaled(new Vector3(thickness, yThickness, depth)),
                new Vector3(x, 0.03f, depth / 2f)));
        }
        for (int j = 0; j < hCount; j++)
        {
            float z = j * step;
            lineMultiMesh.SetInstanceTransform(idx++, new Transform3D(
                Basis.Identity.Scaled(new Vector3(width, yThickness, thickness)),
                new Vector3(width / 2f, 0.03f, z)));
        }
        var gridLinesInstance = new MultiMeshInstance3D
        {
            Multimesh = lineMultiMesh,
            MaterialOverride = new StandardMaterial3D
            {
                AlbedoColor = new Color(1f, 1f, 1f, 0.4f),
                ShadingMode = BaseMaterial3D.ShadingModeEnum.Unshaded,
                Transparency = BaseMaterial3D.TransparencyEnum.Alpha,
            },
        };
        AddChild(gridLinesInstance);
        _gridLines = gridLinesInstance;

        const float pylonStep = 100f;
        var pylonPositions = new List<Vector3>();
        for (float x = pylonStep; x < width; x += pylonStep)
        {
            for (float z = pylonStep; z < depth; z += pylonStep)
            {
                pylonPositions.Add(new Vector3(x, 1.5f, z));
            }
        }
        if (pylonPositions.Count > 0)
        {
            var pylonMultiMesh = new MultiMesh
            {
                TransformFormat = MultiMesh.TransformFormatEnum.Transform3D,
                Mesh = new CylinderMesh { TopRadius = 0.4f, BottomRadius = 0.4f, Height = 3f },
                InstanceCount = pylonPositions.Count,
            };
            for (int i = 0; i < pylonPositions.Count; i++)
            {
                pylonMultiMesh.SetInstanceTransform(i, new Transform3D(Basis.Identity, pylonPositions[i]));
            }
            var pylonsInstance = new MultiMeshInstance3D
            {
                Multimesh = pylonMultiMesh,
                MaterialOverride = new StandardMaterial3D { AlbedoColor = new Color(0.9f, 0.55f, 0.15f) },
            };
            AddChild(pylonsInstance);
            _pylons = pylonsInstance;
        }
        else
        {
            _pylons = null;
        }

        var origin = new MeshInstance3D
        {
            Mesh = new SphereMesh { Radius = 1f, Height = 2f },
            Position = new Vector3(0, 1f, 0),
            MaterialOverride = new StandardMaterial3D
            {
                AlbedoColor = new Color(1f, 0.15f, 0.15f),
                EmissionEnabled = true,
                Emission = new Color(1f, 0.2f, 0.2f),
            },
        };
        AddChild(origin);
        _originMarker = origin;
    }

    // --- Session message handlers ---

    private void HandleJoined(WSession.Joined msg)
    {
        var gs = GameState.Instance;
        gs.EntityId = msg.EntityId;
        if (string.IsNullOrEmpty(gs.ZoneId))
        {
            gs.ZoneId = DefaultInitialZoneId;
        }
        SizeFloorForZone(gs.ZoneId);
        PredictedMovement.HardSet(msg.X, msg.Y, msg.Tick);
        ApplyServerPosition(_selfVisual, msg.X, msg.Y);

        ClearAllVisuals();
        gs.Roster.Clear();
        foreach (var entry in msg.Roster)
        {
            SpawnOrUpdateRosterVisual(entry.EntityId, entry.EntityType, entry.X, entry.Y);
        }
    }

    private void HandleZoneChanged(WSession.ZoneChanged msg)
    {
        var gs = GameState.Instance;
        gs.ZoneId = msg.ZoneId;
        SizeFloorForZone(msg.ZoneId);
        PredictedMovement.HardSet(msg.X, msg.Y, msg.Tick);
        ApplyServerPosition(_selfVisual, msg.X, msg.Y);

        ClearAllVisuals();
        gs.Roster.Clear();
        foreach (var entry in msg.Roster)
        {
            SpawnOrUpdateRosterVisual(entry.EntityId, entry.EntityType, entry.X, entry.Y);
        }
    }

    private void HandleEntitySpawned(WSession.EntitySpawned msg)
    {
        SpawnOrUpdateRosterVisual(msg.EntityId, msg.EntityType, msg.X, msg.Y);
    }

    private void HandleEntityDespawned(WSession.EntityDespawned msg)
    {
        GameState.Instance.Roster.Remove(msg.EntityId);
        GameState.Instance.CubeHp.Remove(msg.EntityId);
        if (_visuals.Remove(msg.EntityId, out var v))
        {
            v.Node.QueueFree();
        }
        if (GameState.Instance.CurrentTargetEntityId == msg.EntityId)
        {
            GameState.Instance.CurrentTargetEntityId = null;
        }
    }

    private void HandleMoved(WSession.Moved msg)
    {
        var gs = GameState.Instance;
        double nowMs = Time.GetUnixTimeFromSystem() * 1000.0;

        if (msg.EntityId == gs.EntityId)
        {
            if (msg.Seq != 0)
            {
                PredictedMovement.ReconcileConfirmed(msg.Seq, msg.X, msg.Y, msg.Tick);
            }
            else
            {
                gs.AuthoritativeX = msg.X;
                gs.AuthoritativeY = msg.Y;
                gs.LastTick = msg.Tick;
            }
            return;
        }

        if (gs.Roster.TryGetValue(msg.EntityId, out var entry))
        {
            entry.PrevX = entry.X;
            entry.PrevY = entry.Y;
            entry.X = msg.X;
            entry.Y = msg.Y;
            entry.LastTick = msg.Tick;
            entry.LastUpdateUnixMs = nowMs;
        }

        if (_visuals.TryGetValue(msg.EntityId, out var visual))
        {
            visual.Interpolator.PushSample(msg.X, msg.Y, nowMs);
        }
    }

    private void HandleRejected(WSession.Rejected msg)
    {
        PredictedMovement.ReconcileRejected(msg.Seq, msg.Tick);
    }

    private void HandlePong(WSession.Pong msg)
    {
        var gs = GameState.Instance;
        long nowMs = (long)(Time.GetUnixTimeFromSystem() * 1000.0);
        gs.LastRttMs = nowMs - msg.ClientSentAt;
        gs.LastClockSkewMs = msg.ServerTime - nowMs;
    }

    // Ad-hoc Evil Cube HP text convention (PROMPT.md §7.1) — the one
    // place in this client that parses PluginMessage bodies instead of
    // a structured field, since StatChanged never covers NPCs.
    private void HandlePluginMessage(WSession.PluginMessage msg)
    {
        string body = msg.Body;
        if (!body.StartsWith("cube:"))
        {
            return;
        }
        var parts = body.Split(':');
        if (parts.Length < 3)
        {
            return;
        }
        string entityId = parts[1];
        string kind = parts[2];
        var gs = GameState.Instance;

        if (kind == "hp" && parts.Length >= 4)
        {
            var frac = parts[3].Split('/');
            if (frac.Length == 2 && long.TryParse(frac[0], out var cur) && long.TryParse(frac[1], out var max))
            {
                gs.CubeHp[entityId] = (cur, max, false);
                UpdateHealthBar(entityId);
            }
        }
        else if (kind == "dead")
        {
            var existing = gs.CubeHp.GetValueOrDefault(entityId, (0, 50, false));
            gs.CubeHp[entityId] = (0, existing.Max, true);
            UpdateHealthBar(entityId);
        }
        else if (kind == "respawned" && parts.Length >= 4)
        {
            var hpPart = parts[3].Split(':');
            long max = hpPart.Length >= 2 && long.TryParse(hpPart[1], out var m) ? m : 50;
            gs.CubeHp[entityId] = (max, max, false);
            UpdateHealthBar(entityId);
        }
    }

    private void UpdateHealthBar(string entityId)
    {
        if (!_visuals.TryGetValue(entityId, out var visual) || visual.HealthLabel is null)
        {
            return;
        }
        var (cur, max, dead) = GameState.Instance.CubeHp.GetValueOrDefault(entityId, (0, 0, false));
        visual.HealthLabel.Text = dead ? "DEAD" : $"HP {cur}/{max}";
    }

    // --- Visual entity management ---

    private void SpawnOrUpdateRosterVisual(string entityId, string entityType, double x, double y)
    {
        var gs = GameState.Instance;
        double nowMs = Time.GetUnixTimeFromSystem() * 1000.0;

        if (gs.Roster.TryGetValue(entityId, out var existing))
        {
            existing.X = x;
            existing.Y = y;
            existing.EntityType = entityType;
        }
        else
        {
            gs.Roster[entityId] = new RosterEntry { EntityId = entityId, EntityType = entityType, X = x, Y = y, PrevX = x, PrevY = y };
        }

        if (_visuals.ContainsKey(entityId))
        {
            _visuals[entityId].Interpolator.HardSet(x, y, nowMs);
            return;
        }

        // §7.2 step 5's original guidance: filter on entity_type ==
        // "npc.evil_cube". A real server bug (world_zero#239) used to
        // collapse every NPC's wire entity_type to the bare string "npc",
        // which briefly made this filter dead code — fixed now, so the
        // spawn table's real declared entity_type reaches the wire again.
        bool isNpc = entityType.StartsWith("npc");
        bool isCube = entityType == "npc.evil_cube";
        var color = isCube ? new Color(0.85f, 0.15f, 0.15f) : isNpc ? new Color(0.6f, 0.5f, 0.2f) : new Color(0.9f, 0.7f, 0.2f);
        Node3D visualNode = isNpc ? BuildBox(color, isCube ? 1.8f : 1.2f) : BuildCapsule(color);
        visualNode.Name = $"Entity_{entityId}";
        BuildPickable(visualNode, isNpc ? 1.2f : 0.6f).SetMeta("entity_id", entityId);
        AddChild(visualNode);

        var label = new Label3D
        {
            Text = isCube ? "Evil Cube" : entityType,
            Position = new Vector3(0, isNpc ? 2.4f : PlayerHeight + 0.4f, 0),
            FontSize = 32,
            Billboard = BaseMaterial3D.BillboardModeEnum.Enabled,
        };
        visualNode.AddChild(label);

        Label3D? hpLabel = null;
        if (isCube)
        {
            hpLabel = new Label3D
            {
                Text = "HP ?/?",
                Position = new Vector3(0, 2.0f, 0),
                FontSize = 28,
                Modulate = new Color(1f, 0.3f, 0.3f),
                Billboard = BaseMaterial3D.BillboardModeEnum.Enabled,
            };
            visualNode.AddChild(hpLabel);
        }

        var visual = new VisualEntity { Node = visualNode, EntityType = entityType, HealthLabel = hpLabel };
        visual.Interpolator.HardSet(x, y, nowMs);
        _visuals[entityId] = visual;
        ApplyServerPosition(visualNode, x, y);
    }

    private void ClearAllVisuals()
    {
        foreach (var v in _visuals.Values)
        {
            v.Node.QueueFree();
        }
        _visuals.Clear();
    }

    private static void ApplyServerPosition(Node3D node, double x, double y)
    {
        node.Position = new Vector3((float)x, node.Position.Y, (float)y);
    }

    // --- Per-frame: input, prediction, interpolation, camera, ping ---

    public override void _Process(double delta)
    {
        var gs = GameState.Instance;
        if (gs.ConnectionState != ConnectionState.InWorld)
        {
            return;
        }

        HandleMovementInput((float)delta);
        ApplyServerPosition(_selfVisual, gs.PredictedX, gs.PredictedY);

        double nowMs = Time.GetUnixTimeFromSystem() * 1000.0;
        foreach (var (entityId, visual) in _visuals)
        {
            var (ix, iy) = visual.Interpolator.GetInterpolated(nowMs);
            ApplyServerPosition(visual.Node, ix, iy);
        }

        // Camera chases the player from a fixed offset — no orbiting,
        // this is a debug tool, not a game (PROMPT.md's closing note).
        var target = _selfVisual.Position + new Vector3(0, PlayerHeight, 0);
        _camera.Position = _selfVisual.Position + new Vector3(0, 9, 10);
        _camera.LookAt(target, Vector3.Up);

        _pingAccumulator += (float)delta;
        if (_pingAccumulator >= 2.0f)
        {
            _pingAccumulator = 0f;
            gs.LastPingSentAtMs = (long)nowMs;
            NetworkClient.Instance.SendPing(gs.LastPingSentAtMs);
        }
    }

    private void HandleMovementInput(float delta)
    {
        var gs = GameState.Instance;
        if (gs.TextInputActive)
        {
            // Typing in a HUD text field (chat, party/guild target,
            // crafting, admin) — don't read WASD at all while it has
            // focus (see UiHelpers.LockMovementWhileFocused).
            return;
        }

        var dir = Vector2.Zero;
        if (Input.IsKeyPressed(Key.W) || Input.IsKeyPressed(Key.Up)) dir.Y -= 1;
        if (Input.IsKeyPressed(Key.S) || Input.IsKeyPressed(Key.Down)) dir.Y += 1;
        if (Input.IsKeyPressed(Key.A) || Input.IsKeyPressed(Key.Left)) dir.X -= 1;
        if (Input.IsKeyPressed(Key.D) || Input.IsKeyPressed(Key.Right)) dir.X += 1;

        if (dir != Vector2.Zero)
        {
            dir = dir.Normalized();
            gs.PredictedX += dir.X * MoveSpeedMps * delta;
            gs.PredictedY += dir.Y * MoveSpeedMps * delta;
        }

        _sendAccumulator += delta;
        if (_sendAccumulator < MoveSendIntervalSec)
        {
            return;
        }
        _sendAccumulator = 0f;

        var current = new Vector2((float)gs.PredictedX, (float)gs.PredictedY);
        if (current.IsEqualApprox(_lastSentPos))
        {
            return;
        }
        _lastSentPos = current;
        NetworkClient.Instance.SendMove(gs.PredictedX, gs.PredictedY, out uint seq);
        PredictedMovement.RecordPredictedMove(seq, gs.PredictedX, gs.PredictedY);
    }

    public override void _UnhandledInput(InputEvent @event)
    {
        if (@event is InputEventMouseButton { ButtonIndex: MouseButton.Left, Pressed: true } mb)
        {
            TryPick(mb.Position);
        }
        else if (@event is InputEventKey { Pressed: true, Keycode: Key.Space })
        {
            var target = GameState.Instance.CurrentTargetEntityId;
            if (target is not null)
            {
                NetworkClient.Instance.SendAttack(target, "hp");
            }
        }
    }

    private void TryPick(Vector2 screenPos)
    {
        var spaceState = GetWorld3D().DirectSpaceState;
        var from = _camera.ProjectRayOrigin(screenPos);
        var to = from + _camera.ProjectRayNormal(screenPos) * 1000f;
        var query = PhysicsRayQueryParameters3D.Create(from, to);
        query.CollideWithAreas = true;
        query.CollideWithBodies = false;
        var result = spaceState.IntersectRay(query);
        if (result.Count == 0)
        {
            return;
        }
        if (result["collider"].As<Area3D>() is { } area && area.HasMeta("entity_id"))
        {
            GameState.Instance.CurrentTargetEntityId = area.GetMeta("entity_id").AsString();
            GameState.Instance.LogEvent("ui", $"targeted {GameState.Instance.CurrentTargetEntityId}");
        }
    }
}
