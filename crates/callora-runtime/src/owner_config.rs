//! The parts of a business's configuration the owner changes from the dashboard's behavior
//! page: the agent's instructions, the fixed sentences, the words the system listens for, the
//! timings and limits. Kept as a patch over the business file (only what was changed), in
//! `callora_v2.business_settings` under `config`; the file stays the default, so resetting an
//! item shows the file's current value. A patch is applied by building the business again from
//! the patched configuration: whatever the file's validation refuses is refused here too.

use serde_json::{Map, Value};

/// What may be changed, as dotted paths into the business JSON; `*` is any one key.
const EDITABLE: &[&str] = &[
    "agent.persona",
    "agent.rules",
    "agent.timeout_ms",
    "agent.filler_after_ms",
    "responses.*.variants",
    "lexicon.*",
    "meta_intents.*.exact",
    "meta_intents.*.phrases",
    "silence.reprompt_after_ms",
    "silence.max_reprompts",
    "silence.patient_after_ms",
    "silence.patient_reprompts",
    "voice.tempo",
    "slots.passengers.max",
    "rules",
    "service_area",
    "stt_keyterms",
    "personal_places",
    "informal_places",
];

/// Whether `path` is one of the items the owner may change.
pub fn editable(path: &str) -> bool {
    let parts: Vec<&str> = path.split('.').collect();
    EDITABLE.iter().any(|pattern| {
        let want: Vec<&str> = pattern.split('.').collect();
        want.len() == parts.len() && want.iter().zip(&parts).all(|(w, p)| !p.is_empty() && (*w == "*" || w == p))
    })
}

/// The patch laid over `base`: objects merged key by key, anything else replaced.
pub fn merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                merge(b.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (b, p) => *b = p.clone(),
    }
}

/// Sets `path` in the patch to `value`, or removes it (back to the file's value), dropping
/// objects left empty.
pub fn set(patch: &mut Value, path: &str, value: Option<Value>) {
    if !patch.is_object() {
        *patch = Value::Object(Map::new());
    }
    let parts: Vec<&str> = path.split('.').collect();
    set_in(patch.as_object_mut().expect("an object"), &parts, value);
}

fn set_in(obj: &mut Map<String, Value>, parts: &[&str], value: Option<Value>) {
    let [first, rest @ ..] = parts else { return };
    if rest.is_empty() {
        match value {
            Some(v) => {
                obj.insert((*first).to_string(), v);
            }
            None => {
                obj.remove(*first);
            }
        }
        return;
    }
    if value.is_none() && !obj.contains_key(*first) {
        return;
    }
    let child = obj.entry((*first).to_string()).or_insert_with(|| Value::Object(Map::new()));
    if !child.is_object() {
        *child = Value::Object(Map::new());
    }
    let inner = child.as_object_mut().expect("an object");
    set_in(inner, rest, value);
    if inner.is_empty() {
        obj.remove(*first);
    }
}

/// The dotted paths the patch changes, each down to an editable item.
pub fn changed(patch: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(patch, String::new(), &mut out);
    out
}

fn walk(v: &Value, at: String, out: &mut Vec<String>) {
    if !at.is_empty() && editable(&at) {
        out.push(at);
        return;
    }
    if let Value::Object(o) = v {
        for (k, child) in o {
            let path = if at.is_empty() { k.clone() } else { format!("{at}.{k}") };
            walk(child, path, out);
        }
    }
}

/// The editable parts of a configuration, for the page: what is shown and can be changed.
pub fn view(config: &Value) -> Value {
    let pick =
        |path: &str| -> Value { path.split('.').try_fold(config, |v, k| v.get(k)).cloned().unwrap_or(Value::Null) };
    let responses: Map<String, Value> = config["responses"]
        .as_object()
        .map(|r| r.iter().map(|(id, v)| (id.clone(), v["variants"].clone())).collect())
        .unwrap_or_default();
    let meta: Map<String, Value> = config["meta_intents"]
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(id, v)| {
                    (id.clone(), serde_json::json!({ "exact": v.get("exact"), "phrases": v.get("phrases") }))
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "agent": {
            "persona": pick("agent.persona"),
            "rules": pick("agent.rules"),
            "timeout_ms": pick("agent.timeout_ms"),
            "filler_after_ms": pick("agent.filler_after_ms"),
        },
        "responses": responses,
        "lexicon": pick("lexicon"),
        "meta_intents": meta,
        "silence": pick("silence"),
        "voice": { "tempo": pick("voice.tempo") },
        "slots": { "passengers": { "max": pick("slots.passengers.max") } },
        "rules": pick("rules"),
        "service_area": pick("service_area"),
        "stt_keyterms": pick("stt_keyterms"),
        "personal_places": pick("personal_places"),
        "informal_places": pick("informal_places"),
    })
}

/// Whether a change at `path` changes what is said in recorded clips (their words or pace):
/// the voice library is recorded again for what is new.
pub fn needs_recording(path: &str) -> bool {
    path.starts_with("responses.") || path == "voice.tempo"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_the_listed_items_are_editable() {
        assert!(editable("agent.persona"));
        assert!(editable("responses.greeting.variants"));
        assert!(editable("lexicon.abuse"));
        assert!(editable("meta_intents.goodbye.exact"));
        assert!(!editable("responses.greeting"));
        assert!(!editable("actions"));
        assert!(!editable("voice.voice_id_env"));
        assert!(!editable("lexicon"));
        assert!(!editable("lexicon..x"));
    }

    #[test]
    fn the_taxi_file_is_built_again_from_its_config_with_a_change() {
        use callora_core::business::Business;
        let env = |k: &str| (k == "TAXI_PHONE_NUMBERS").then(|| "+972500000000".to_string());
        let file = Business::from_json(include_str!("../../../businesses/taxi.json"), "taxi.json", &env).unwrap();
        let mut config = serde_json::to_value(&file.config).unwrap();
        let same = Business::from_json(&config.to_string(), "dashboard", &env).expect("the file, as it is");
        assert_eq!(callora_core::agent::system_prompt(&same), callora_core::agent::system_prompt(&file));
        assert_eq!(same.phone_numbers, file.phone_numbers);

        let mut patch = json!({});
        set(&mut patch, "agent.rules", Some(json!(["תמיד לומר שלום."])));
        set(&mut patch, "lexicon.abuse", Some(json!(["מילה"])));
        merge(&mut config, &patch);
        let changed = Business::from_json(&config.to_string(), "dashboard", &env).expect("a valid change");
        assert!(callora_core::agent::system_prompt(&changed).contains("תמיד לומר שלום."));
        assert!(changed.is_abusive("איזו מילה"));

        // What the file's validation refuses is refused: a sentence with no words.
        let mut bad = serde_json::to_value(&file.config).unwrap();
        merge(&mut bad, &json!({ "responses": { "greeting": { "variants": [] } } }));
        assert!(Business::from_json(&bad.to_string(), "dashboard", &env).is_err());
    }

    #[test]
    fn a_patch_is_set_merged_and_reset() {
        let mut patch = json!({});
        set(&mut patch, "agent.persona", Some(json!("אחר")));
        set(&mut patch, "responses.greeting.variants", Some(json!(["שלום"])));
        assert_eq!(changed(&patch), vec!["agent.persona", "responses.greeting.variants"]);

        let mut base = json!({ "agent": { "persona": "x", "rules": ["a"] }, "responses": { "greeting": { "variants": ["היי"], "delivery": "quick" } } });
        merge(&mut base, &patch);
        assert_eq!(base["agent"]["persona"], "אחר");
        assert_eq!(base["agent"]["rules"], json!(["a"]), "the rest of the file stays");
        assert_eq!(base["responses"]["greeting"], json!({ "variants": ["שלום"], "delivery": "quick" }));

        set(&mut patch, "responses.greeting.variants", None);
        set(&mut patch, "agent.persona", None);
        assert_eq!(patch, json!({}), "nothing left once every item is reset");
        set(&mut patch, "lexicon.abuse", None);
        assert_eq!(patch, json!({}));
    }
}
