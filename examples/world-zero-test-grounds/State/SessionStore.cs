using System.Collections.Generic;
using Godot;

namespace WorldZeroTestGrounds.State;

// Persists session_tokens to user:// so a restarted client can
// demonstrate Resume{session_token} (PROMPT.md §2.4/§18 step 17) instead
// of only ever exercising Login. Plain text is fine here — this is a
// disposable local dev test client, not a real account store.
//
// Keyed by username, not a single "last session" slot — Godot's user://
// data dir is shared by every instance of this project running on the
// same machine (there's no per-instance isolation), so two client
// windows testing two different accounts side by side were overwriting
// EACH OTHER's "last session," making Resume silently log into whichever
// account last authenticated in *either* window. A per-username entry
// means each account's own token survives regardless of what the other
// client does.
public static class SessionStore
{
    private const string Path = "user://sessions.cfg";
    private const string Section = "sessions";

    public static void Save(string username, string sessionToken)
    {
        var cfg = new ConfigFile();
        cfg.Load(Path); // best-effort — merge into whatever's already there
        cfg.SetValue(Section, username, sessionToken);
        cfg.Save(Path);
    }

    public static Dictionary<string, string> LoadAll()
    {
        var result = new Dictionary<string, string>();
        var cfg = new ConfigFile();
        if (cfg.Load(Path) != Error.Ok)
        {
            return result;
        }
        foreach (var key in cfg.GetSectionKeys(Section))
        {
            string username = key;
            string token = (string)cfg.GetValue(Section, key, "");
            if (!string.IsNullOrEmpty(token))
            {
                result[username] = token;
            }
        }
        return result;
    }

    public static void Remove(string username)
    {
        var cfg = new ConfigFile();
        if (cfg.Load(Path) != Error.Ok)
        {
            return;
        }
        if (cfg.HasSectionKey(Section, username))
        {
            cfg.EraseSectionKey(Section, username);
            cfg.Save(Path);
        }
    }
}
