using System.Collections.Generic;
using System.IO;
using Godot;

namespace WorldZeroTestGrounds.State;

// PROMPT.md §9.3 — this client's own developer-local config, distinct
// from world_zero's own `.env` (the backend's Postgres/Redis config,
// which this project never touches). Godot has no built-in .env
// loader, so this is a tiny key=value parser reading `.env` from the
// project root at startup, falling back to `.env.example`'s documented
// defaults when a key (or the file) is missing.
public partial class EnvConfig : Node
{
    public static EnvConfig Instance { get; private set; } = null!;

    private readonly Dictionary<string, string> _values = new();

    public string ServerHost => Get("WZ_TEST_SERVER_HOST", "127.0.0.1");
    public int ServerPort => int.TryParse(Get("WZ_TEST_SERVER_PORT", "7900"), out var p) ? p : 7900;
    public string DefaultUsername => Get("WZ_TEST_DEFAULT_USERNAME", "");
    public string DefaultPassword => Get("WZ_TEST_DEFAULT_PASSWORD", "");
    public string TlsCertPath => Get("WZ_TEST_TLS_CERT_PATH", "");

    public override void _EnterTree()
    {
        Instance = this;
        Load();
    }

    private void Load()
    {
        string path = ProjectSettings.GlobalizePath("res://.env");
        if (!File.Exists(path))
        {
            GD.Print("[EnvConfig] no .env found at project root — copy .env.example to .env and fill in WZ_TEST_REALM_ID at minimum.");
            return;
        }

        foreach (var rawLine in File.ReadAllLines(path))
        {
            string line = rawLine.Trim();
            if (line.Length == 0 || line.StartsWith('#'))
            {
                continue;
            }
            int eq = line.IndexOf('=');
            if (eq < 0)
            {
                continue;
            }
            string key = line[..eq].Trim();
            string value = line[(eq + 1)..].Trim();
            // Strip optional surrounding quotes, same convention most
            // .env tooling accepts.
            if (value.Length >= 2 && ((value[0] == '"' && value[^1] == '"') || (value[0] == '\'' && value[^1] == '\'')))
            {
                value = value[1..^1];
            }
            _values[key] = value;
        }
    }

    private string Get(string key, string fallback) => _values.GetValueOrDefault(key, fallback);
}
