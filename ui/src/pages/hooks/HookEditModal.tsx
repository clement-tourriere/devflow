import { useState, useRef, useEffect } from "react";
import type { ActionTypeInfo, ActionFieldInfo, HookInfo, RecipeInfo } from "../../types";
import { saveHooks, listHooks, installRecipe } from "../../utils/invoke";

interface ConditionPreset {
  label: string;
  value: string;
  description: string;
  hasParam?: boolean;
  paramPlaceholder?: string;
  category: string;
}

const CONDITION_PRESETS: ConditionPreset[] = [
  // Worktree
  { label: "Is worktree", value: "is_worktree", description: "Only run when workspace uses a worktree", category: "Worktree" },
  { label: "Not worktree", value: "not_worktree", description: "Only run when workspace does NOT use a worktree", category: "Worktree" },
  // Trigger
  { label: "Trigger is...", value: "trigger_is:", description: "Only run when triggered by a specific source", hasParam: true, paramPlaceholder: "vcs, cli, gui, auto", category: "Trigger" },
  { label: "Trigger is not...", value: "trigger_not:", description: "Skip when triggered by a specific source", hasParam: true, paramPlaceholder: "vcs, cli, gui, auto", category: "Trigger" },
  // Workspace
  { label: "Workspace is...", value: "workspace_is:", description: "Only run for a specific workspace name", hasParam: true, paramPlaceholder: "workspace name", category: "Workspace" },
  { label: "Workspace is not...", value: "workspace_not:", description: "Skip for a specific workspace name", hasParam: true, paramPlaceholder: "workspace name", category: "Workspace" },
  { label: "Workspace matches...", value: "workspace_matches:", description: "Workspace name matches a regex pattern", hasParam: true, paramPlaceholder: "^feature/.*", category: "Workspace" },
  { label: "Is default workspace", value: "is_default_workspace", description: "Only run on the default workspace (e.g. main)", category: "Workspace" },
  { label: "Not default workspace", value: "not_default_workspace", description: "Skip the default workspace", category: "Workspace" },
  // File system
  { label: "File exists...", value: "file_exists:", description: "Only run if a file exists", hasParam: true, paramPlaceholder: "package.json", category: "File System" },
  { label: "Directory exists...", value: "dir_exists:", description: "Only run if a directory exists", hasParam: true, paramPlaceholder: "node_modules", category: "File System" },
  // Environment
  { label: "Env var is set...", value: "env_set:", description: "Only run if an environment variable is set", hasParam: true, paramPlaceholder: "CI", category: "Environment" },
  { label: "Env var equals...", value: "env_is:", description: "Only run if an env var has a specific value", hasParam: true, paramPlaceholder: "NODE_ENV=production", category: "Environment" },
  // Boolean
  { label: "Always", value: "always", description: "Always run (useful for testing)", category: "Other" },
  { label: "Never", value: "never", description: "Never run (useful for disabling)", category: "Other" },
];

interface Props {
  projectPath: string;
  phase: string;
  actionTypes: ActionTypeInfo[];
  editingHook?: HookInfo;
  onClose: () => void;
  onSaved: () => void;
  recipes?: RecipeInfo[];
}

type Step = "pick-type" | "configure";

/** Extract initial form state from an existing hook's raw data for editing. */
function extractEditState(
  hook: HookInfo,
  actionTypes: ActionTypeInfo[]
): {
  selectedType: ActionTypeInfo | null;
  hookName: string;
  fieldValues: Record<string, unknown>;
  condition: string;
  continueOnError: boolean;
  background: boolean;
} {
  const raw = hook.raw as Record<string, unknown> | string;
  const hookName = hook.name;
  const condition = hook.condition || "";
  const background = hook.background || false;

  // Simple hook: raw is just the command string
  if (typeof raw === "string") {
    const shellType = actionTypes.find((t) => t.type === "shell") || null;
    return {
      selectedType: shellType,
      hookName,
      fieldValues: { command: raw },
      condition,
      continueOnError: false,
      background,
    };
  }

  // Extended hook (has "command" key, no "action" key)
  if (raw && "command" in raw && !("action" in raw)) {
    const shellType = actionTypes.find((t) => t.type === "shell") || null;
    return {
      selectedType: shellType,
      hookName,
      fieldValues: { command: raw.command as string },
      condition: (raw.condition as string) || condition,
      continueOnError: !!(raw.continue_on_error),
      background: !!(raw.background) || background,
    };
  }

  // Action hook (has "action" key)
  if (raw && "action" in raw) {
    const action = raw.action as Record<string, unknown>;
    const actionTypeName = action.type as string;
    const actionType = actionTypes.find((t) => t.type === actionTypeName) || null;

    const fieldValues: Record<string, unknown> = {};
    if (actionType) {
      for (const field of actionType.fields) {
        if (action[field.name] !== undefined) {
          fieldValues[field.name] = action[field.name];
        } else if (field.default_value !== undefined) {
          fieldValues[field.name] =
            field.field_type === "bool"
              ? field.default_value === "true"
              : field.default_value;
        } else if (field.field_type === "key-value") {
          fieldValues[field.name] = {};
        } else if (field.field_type === "bool") {
          fieldValues[field.name] = false;
        } else {
          fieldValues[field.name] = "";
        }
      }
    }

    return {
      selectedType: actionType,
      hookName,
      fieldValues,
      condition: (raw.condition as string) || condition,
      continueOnError: !!(raw.continue_on_error),
      background: !!(raw.background) || background,
    };
  }

  // Fallback
  return {
    selectedType: null,
    hookName,
    fieldValues: {},
    condition,
    continueOnError: false,
    background,
  };
}

export default function HookEditModal({
  projectPath,
  phase,
  actionTypes,
  editingHook,
  onClose,
  onSaved,
  recipes,
}: Props) {
  const isEditing = !!editingHook;
  const editState = editingHook
    ? extractEditState(editingHook, actionTypes)
    : null;

  const [step, setStep] = useState<Step>(
    isEditing ? "configure" : "pick-type"
  );
  const [selectedType, setSelectedType] = useState<ActionTypeInfo | null>(
    editState?.selectedType ?? null
  );
  const [hookName, setHookName] = useState(editState?.hookName ?? "");
  const [fieldValues, setFieldValues] = useState<Record<string, unknown>>(
    editState?.fieldValues ?? {}
  );
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [installingRecipeName, setInstallingRecipeName] = useState<string | null>(null);
  const [showDiscardConfirm, setShowDiscardConfirm] = useState(false);

  // Common hook options
  const [condition, setCondition] = useState(editState?.condition ?? "");
  const [continueOnError, setContinueOnError] = useState(
    editState?.continueOnError ?? false
  );
  const [background, setBackground] = useState(
    editState?.background ?? false
  );

  // Track whether the form has been touched
  const hasUnsavedChanges = () => {
    if (step === "pick-type") return false;
    if (isEditing) {
      // Editing: compare against initial state
      if (hookName !== (editState?.hookName ?? "")) return true;
      if (condition !== (editState?.condition ?? "")) return true;
      if (continueOnError !== (editState?.continueOnError ?? false)) return true;
      if (background !== (editState?.background ?? false)) return true;
      if (JSON.stringify(fieldValues) !== JSON.stringify(editState?.fieldValues ?? {})) return true;
      return false;
    }
    // New hook: any content means unsaved
    return hookName.trim() !== "" || Object.values(fieldValues).some((v) => v !== "" && v !== false);
  };

  const handleCloseAttempt = () => {
    if (hasUnsavedChanges()) {
      setShowDiscardConfirm(true);
      return;
    }
    onClose();
  };

  const handleTypeSelect = (actionType: ActionTypeInfo) => {
    setSelectedType(actionType);
    // Initialize defaults
    const defaults: Record<string, unknown> = {};
    for (const field of actionType.fields) {
      if (field.default_value !== undefined) {
        if (field.field_type === "bool") {
          defaults[field.name] = field.default_value === "true";
        } else {
          defaults[field.name] = field.default_value;
        }
      } else if (field.field_type === "key-value") {
        defaults[field.name] = {};
      } else if (field.field_type === "bool") {
        defaults[field.name] = false;
      } else {
        defaults[field.name] = "";
      }
    }
    setFieldValues(defaults);
    setStep("configure");
  };

  const handleSave = async () => {
    if (!hookName.trim()) {
      setError("Hook name is required");
      return;
    }
    if (!selectedType) return;

    setSaving(true);
    setError(null);

    try {
      // Build the action object
      const action: Record<string, unknown> = { type: selectedType.type };
      for (const field of selectedType.fields) {
        const val = fieldValues[field.name];
        if (val !== undefined && val !== "" && val !== false) {
          action[field.name] = val;
        }
      }

      // Build the hook entry
      const hookEntry: Record<string, unknown> = { action };
      if (condition.trim()) hookEntry.condition = condition;
      if (continueOnError) hookEntry.continue_on_error = true;
      if (background) hookEntry.background = true;

      // Fetch current hooks and rebuild the object using raw data
      const currentPhases = await listHooks(projectPath);
      const hooksObj: Record<string, Record<string, unknown>> = {};

      for (const p of currentPhases) {
        hooksObj[p.phase] = {};
        for (const h of p.hooks) {
          // When editing, skip the old hook entry (we'll add the updated one)
          if (
            isEditing &&
            p.phase === phase &&
            h.name === editingHook?.name
          ) {
            continue;
          }
          hooksObj[p.phase][h.name] = h.raw as Record<string, unknown>;
        }
      }

      // Add or create phase
      if (!hooksObj[phase]) {
        hooksObj[phase] = {};
      }
      hooksObj[phase][hookName.trim()] = hookEntry;

      await saveHooks(projectPath, hooksObj);
      onSaved();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const updateField = (name: string, value: unknown) => {
    setFieldValues((prev) => ({ ...prev, [name]: value }));
  };

  const renderField = (field: ActionFieldInfo) => {
    const value = fieldValues[field.name];

    switch (field.field_type) {
      case "string":
        return (
          <TemplateFieldWrapper
            template={field.template}
            onInsert={(snippet) => {
              updateField(field.name, ((value as string) || "") + snippet);
            }}
          >
            <input
              type="text"
              value={(value as string) || ""}
              onChange={(e) => updateField(field.name, e.target.value)}
              placeholder={field.template ? "Supports {{ templates }}" : ""}
              className="mono"
              style={{ fontSize: 12, width: "100%" }}
            />
          </TemplateFieldWrapper>
        );

      case "text":
        return (
          <TemplateFieldWrapper
            template={field.template}
            onInsert={(snippet) => {
              updateField(field.name, ((value as string) || "") + snippet);
            }}
          >
            <textarea
              value={(value as string) || ""}
              onChange={(e) => updateField(field.name, e.target.value)}
              placeholder={field.template ? "Supports {{ templates }}" : ""}
              className="mono"
              style={{ fontSize: 12, minHeight: 60, width: "100%" }}
            />
          </TemplateFieldWrapper>
        );

      case "bool":
        return (
          <label style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 13 }}>
            <input
              type="checkbox"
              checked={!!value}
              onChange={(e) => updateField(field.name, e.target.checked)}
            />
            {field.label}
          </label>
        );

      case "select":
        return (
          <select
            value={(value as string) || field.default_value || ""}
            onChange={(e) => updateField(field.name, e.target.value)}
            style={{ fontSize: 12, width: "100%" }}
          >
            {field.options?.map((opt) => (
              <option key={opt} value={opt}>
                {opt}
              </option>
            ))}
          </select>
        );

      case "key-value":
        return <KeyValueEditor value={(value as Record<string, string>) || {}} onChange={(v) => updateField(field.name, v)} />;

      default:
        return null;
    }
  };

  return (
    <div
      style={{
        position: "fixed",
        top: 0,
        left: 0,
        right: 0,
        bottom: 0,
        background: "rgba(0,0,0,0.5)",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        zIndex: 1000,
      }}
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) {
          e.preventDefault();
          e.stopPropagation();
          handleCloseAttempt();
        }
      }}
    >
      <div
        style={{
          position: "relative",
          background: "var(--bg-secondary)",
          borderRadius: 12,
          border: "1px solid var(--border)",
          width: step === "pick-type" ? 640 : 520,
          maxHeight: "85vh",
          overflow: "auto",
          padding: 24,
        }}
      >
        {step === "pick-type" && (
          <>
            <h2 style={{ margin: "0 0 4px", fontSize: 18 }}>
              {isEditing ? `Edit Hook: ${editingHook?.name}` : `Add Hook to ${phase}`}
            </h2>
            <p style={{ color: "var(--text-secondary)", fontSize: 13, marginBottom: 16 }}>
              Choose an action type:
            </p>

            {/* From Recipe section */}
            {(() => {
              const phaseRecipes = (recipes || []).filter((r) =>
                r.hooks_preview.some((h) => h.phase === phase)
              );
              if (phaseRecipes.length === 0) return null;
              return (
                <div style={{ marginBottom: 12 }}>
                  <div
                    style={{
                      fontSize: 11,
                      textTransform: "uppercase",
                      color: "var(--text-muted)",
                      fontWeight: 600,
                      marginBottom: 6,
                    }}
                  >
                    From Recipe
                  </div>
                  <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
                    {phaseRecipes.map((recipe) => (
                      <div
                        key={recipe.name}
                        style={{
                          padding: "10px 16px",
                          border: "1px solid var(--border)",
                          borderRadius: 8,
                          background: "var(--bg-primary)",
                          display: "flex",
                          justifyContent: "space-between",
                          alignItems: "center",
                        }}
                      >
                        <div>
                          <div style={{ fontWeight: 600, fontSize: 13 }}>{recipe.name}</div>
                          <div style={{ fontSize: 12, color: "var(--text-secondary)" }}>
                            {recipe.description}
                          </div>
                        </div>
                        <button
                          className="btn btn-primary"
                          style={{ fontSize: 11, padding: "4px 12px", flexShrink: 0, marginLeft: 12 }}
                          disabled={installingRecipeName !== null}
                          onClick={async () => {
                            setInstallingRecipeName(recipe.name);
                            try {
                              await installRecipe(projectPath, recipe.name);
                              onSaved();
                            } catch (e) {
                              setError(String(e));
                            } finally {
                              setInstallingRecipeName(null);
                            }
                          }}
                        >
                          {installingRecipeName === recipe.name ? "Installing..." : "Install Recipe"}
                        </button>
                      </div>
                    ))}
                  </div>
                  <div
                    style={{
                      fontSize: 11,
                      textTransform: "uppercase",
                      color: "var(--text-muted)",
                      fontWeight: 600,
                      marginTop: 12,
                      marginBottom: 6,
                    }}
                  >
                    Or choose an action type
                  </div>
                </div>
              );
            })()}

            <div
              style={{
                display: "grid",
                gridTemplateColumns: "1fr 1fr",
                gap: 8,
              }}
            >
              {actionTypes.map((at) => (
                <div
                  key={at.type}
                  onClick={() => handleTypeSelect(at)}
                  style={{
                    padding: "12px 16px",
                    border: "1px solid var(--border)",
                    borderRadius: 8,
                    cursor: "pointer",
                    background: "var(--bg-primary)",
                    transition: "border-color 0.15s",
                  }}
                  onMouseEnter={(e) =>
                    (e.currentTarget.style.borderColor = "var(--accent)")
                  }
                  onMouseLeave={(e) =>
                    (e.currentTarget.style.borderColor = "var(--border)")
                  }
                >
                  <div style={{ fontWeight: 600, fontSize: 13, marginBottom: 2 }}>
                    {at.label}
                  </div>
                  <div style={{ fontSize: 12, color: "var(--text-secondary)" }}>
                    {at.description}
                  </div>
                  {at.requires_approval && (
                    <span
                      className="badge badge-warning"
                      style={{ fontSize: 10, marginTop: 4 }}
                    >
                      requires approval
                    </span>
                  )}
                </div>
              ))}
            </div>
            <div style={{ marginTop: 16, textAlign: "right" }}>
              <button className="btn" onClick={handleCloseAttempt}>
                Cancel
              </button>
            </div>
          </>
        )}

        {step === "configure" && selectedType && (
          <>
            <h2 style={{ margin: "0 0 16px", fontSize: 18 }}>
              {isEditing ? `Edit: ${selectedType.label}` : `Configure: ${selectedType.label}`}
            </h2>

            <div style={{ marginBottom: 12 }}>
              <label style={{ fontSize: 12, fontWeight: 600, display: "block", marginBottom: 4 }}>
                Hook Name *
              </label>
              <input
                type="text"
                value={hookName}
                onChange={(e) => setHookName(e.target.value)}
                placeholder="e.g. update-env, install-deps"
                style={{ fontSize: 12, width: "100%" }}
              />
            </div>

            <div
              style={{
                borderTop: "1px solid var(--border)",
                paddingTop: 12,
                marginTop: 12,
                marginBottom: 12,
              }}
            >
              <div
                style={{
                  fontSize: 11,
                  textTransform: "uppercase",
                  color: "var(--text-muted)",
                  marginBottom: 8,
                  fontWeight: 600,
                }}
              >
                Action Parameters
              </div>
              {selectedType.fields.map((field) => {
                if (field.field_type === "bool") {
                  return (
                    <div key={field.name} style={{ marginBottom: 8 }}>
                      {renderField(field)}
                    </div>
                  );
                }
                return (
                  <div key={field.name} style={{ marginBottom: 8 }}>
                    <label
                      style={{
                        fontSize: 12,
                        fontWeight: 600,
                        display: "block",
                        marginBottom: 4,
                      }}
                    >
                      {field.label}
                      {field.required && " *"}
                      {field.template && (
                        <span
                          className="mono"
                          style={{
                            fontWeight: 400,
                            color: "var(--accent)",
                            marginLeft: 6,
                            fontSize: 11,
                          }}
                          title="Supports MiniJinja templates"
                        >
                          {"{ }"}
                        </span>
                      )}
                    </label>
                    {renderField(field)}
                  </div>
                );
              })}
            </div>

            <div
              style={{
                borderTop: "1px solid var(--border)",
                paddingTop: 12,
                marginBottom: 12,
              }}
            >
              <div
                style={{
                  fontSize: 11,
                  textTransform: "uppercase",
                  color: "var(--text-muted)",
                  marginBottom: 8,
                  fontWeight: 600,
                }}
              >
                Options
              </div>
              <div style={{ marginBottom: 8 }}>
                <label style={{ fontSize: 12, fontWeight: 600, display: "block", marginBottom: 4 }}>
                  Condition
                </label>
                <ConditionPicker value={condition} onChange={setCondition} />
              </div>
              <label style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 13, marginBottom: 4 }}>
                <input
                  type="checkbox"
                  checked={continueOnError}
                  onChange={(e) => setContinueOnError(e.target.checked)}
                />
                Continue on error
              </label>
              <label style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 13 }}>
                <input
                  type="checkbox"
                  checked={background}
                  onChange={(e) => setBackground(e.target.checked)}
                />
                Run in background
              </label>
            </div>

            {error && (
              <div
                style={{
                  padding: 8,
                  marginBottom: 12,
                  background: "rgba(248, 81, 73, 0.1)",
                  border: "1px solid var(--danger)",
                  borderRadius: 6,
                  color: "var(--danger)",
                  fontSize: 12,
                }}
              >
                {error}
              </div>
            )}

            <div style={{ display: "flex", justifyContent: isEditing ? "flex-end" : "space-between" }}>
              {!isEditing && (
                <button className="btn" onClick={() => setStep("pick-type")}>
                  &larr; Back
                </button>
              )}
              <div style={{ display: "flex", gap: 8 }}>
                <button className="btn" onClick={handleCloseAttempt}>
                  Cancel
                </button>
                <button
                  className="btn btn-primary"
                  onClick={handleSave}
                  disabled={saving}
                >
                  {saving ? "Saving..." : isEditing ? "Save Changes" : "Add Hook"}
                </button>
              </div>
            </div>
          </>
        )}

        {/* Discard confirmation overlay */}
        {showDiscardConfirm && (
          <div
            style={{
              position: "absolute",
              inset: 0,
              background: "rgba(0,0,0,0.6)",
              borderRadius: 12,
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
              zIndex: 10,
            }}
          >
            <div
              style={{
                background: "var(--bg-secondary)",
                border: "1px solid var(--border)",
                borderRadius: 10,
                padding: "20px 24px",
                textAlign: "center",
                maxWidth: 300,
              }}
            >
              <div style={{ fontSize: 14, fontWeight: 600, marginBottom: 6 }}>
                Discard changes?
              </div>
              <div style={{ fontSize: 12, color: "var(--text-secondary)", marginBottom: 16 }}>
                You have unsaved changes that will be lost.
              </div>
              <div style={{ display: "flex", gap: 8, justifyContent: "center" }}>
                <button
                  className="btn"
                  onClick={() => setShowDiscardConfirm(false)}
                >
                  Keep Editing
                </button>
                <button
                  className="btn"
                  style={{ background: "var(--danger)", color: "#fff", borderColor: "var(--danger)" }}
                  onClick={onClose}
                >
                  Discard
                </button>
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

function ConditionPicker({
  value,
  onChange,
}: {
  value: string;
  onChange: (v: string) => void;
}) {
  const [mode, setMode] = useState<"none" | "preset" | "custom">(() => {
    if (!value) return "none";
    const match = CONDITION_PRESETS.find(
      (p) => value === p.value || (p.hasParam && value.startsWith(p.value))
    );
    return match ? "preset" : "custom";
  });

  const [selectedPreset, setSelectedPreset] = useState<string>(() => {
    if (!value) return "";
    const match = CONDITION_PRESETS.find(
      (p) => value === p.value || (p.hasParam && value.startsWith(p.value))
    );
    return match ? match.value : "";
  });

  const [paramValue, setParamValue] = useState<string>(() => {
    if (!value) return "";
    const match = CONDITION_PRESETS.find(
      (p) => p.hasParam && value.startsWith(p.value)
    );
    return match ? value.slice(match.value.length) : "";
  });

  const [dropdownOpen, setDropdownOpen] = useState(false);
  const dropdownRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handler = (e: MouseEvent) => {
      if (dropdownRef.current && !dropdownRef.current.contains(e.target as Node)) {
        setDropdownOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, []);

  const handleModeChange = (newMode: "none" | "preset" | "custom") => {
    setMode(newMode);
    if (newMode === "none") {
      onChange("");
      setSelectedPreset("");
      setParamValue("");
    } else if (newMode === "custom") {
      setSelectedPreset("");
      setParamValue("");
      // Keep current value if switching from preset to custom
    } else {
      onChange("");
      setSelectedPreset("");
      setParamValue("");
    }
  };

  const handlePresetSelect = (preset: ConditionPreset) => {
    setSelectedPreset(preset.value);
    setDropdownOpen(false);
    if (preset.hasParam) {
      setParamValue("");
      onChange(preset.value);
    } else {
      setParamValue("");
      onChange(preset.value);
    }
  };

  const handleParamChange = (param: string) => {
    setParamValue(param);
    onChange(selectedPreset + param);
  };

  const activePreset = CONDITION_PRESETS.find((p) => p.value === selectedPreset);

  // Group presets by category
  const categories = Array.from(new Set(CONDITION_PRESETS.map((p) => p.category)));

  return (
    <div>
      <div style={{ display: "flex", gap: 6, marginBottom: 6 }}>
        {(["none", "preset", "custom"] as const).map((m) => (
          <button
            key={m}
            className={`btn${mode === m ? " btn-primary" : ""}`}
            onClick={() => handleModeChange(m)}
            style={{ fontSize: 11, padding: "3px 10px" }}
          >
            {m === "none" ? "No condition" : m === "preset" ? "Built-in" : "Custom"}
          </button>
        ))}
      </div>

      {mode === "preset" && (
        <div ref={dropdownRef} style={{ position: "relative" }}>
          <button
            className="btn"
            onClick={() => setDropdownOpen(!dropdownOpen)}
            style={{
              fontSize: 12,
              width: "100%",
              textAlign: "left",
              display: "flex",
              justifyContent: "space-between",
              alignItems: "center",
              padding: "6px 10px",
            }}
          >
            <span>
              {activePreset ? activePreset.label : "Choose a condition..."}
            </span>
            <span style={{ fontSize: 10, color: "var(--text-muted)" }}>
              {dropdownOpen ? "\u25B2" : "\u25BC"}
            </span>
          </button>

          {dropdownOpen && (
            <div
              style={{
                position: "absolute",
                top: "100%",
                left: 0,
                right: 0,
                zIndex: 100,
                background: "var(--bg-primary)",
                border: "1px solid var(--border)",
                borderRadius: 6,
                maxHeight: 280,
                overflow: "auto",
                marginTop: 2,
                boxShadow: "0 4px 12px rgba(0,0,0,0.15)",
              }}
            >
              {categories.map((cat) => (
                <div key={cat}>
                  <div
                    style={{
                      fontSize: 10,
                      textTransform: "uppercase",
                      color: "var(--text-muted)",
                      padding: "6px 10px 2px",
                      fontWeight: 600,
                    }}
                  >
                    {cat}
                  </div>
                  {CONDITION_PRESETS.filter((p) => p.category === cat).map(
                    (preset) => (
                      <div
                        key={preset.value}
                        onClick={() => handlePresetSelect(preset)}
                        style={{
                          padding: "5px 10px",
                          cursor: "pointer",
                          fontSize: 12,
                          background:
                            selectedPreset === preset.value
                              ? "var(--bg-secondary)"
                              : "transparent",
                        }}
                        onMouseEnter={(e) =>
                          (e.currentTarget.style.background =
                            "var(--bg-secondary)")
                        }
                        onMouseLeave={(e) =>
                          (e.currentTarget.style.background =
                            selectedPreset === preset.value
                              ? "var(--bg-secondary)"
                              : "transparent")
                        }
                      >
                        <div style={{ fontWeight: 500 }}>{preset.label}</div>
                        <div
                          style={{
                            fontSize: 11,
                            color: "var(--text-secondary)",
                          }}
                        >
                          {preset.description}
                        </div>
                      </div>
                    )
                  )}
                </div>
              ))}
            </div>
          )}

          {activePreset?.hasParam && (
            <input
              type="text"
              value={paramValue}
              onChange={(e) => handleParamChange(e.target.value)}
              placeholder={activePreset.paramPlaceholder}
              className="mono"
              style={{ fontSize: 12, width: "100%", marginTop: 6 }}
              autoFocus
            />
          )}

          {activePreset && (
            <div
              className="mono"
              style={{
                fontSize: 11,
                color: "var(--text-secondary)",
                marginTop: 4,
                padding: "4px 8px",
                background: "var(--bg-primary)",
                borderRadius: 4,
              }}
            >
              {value || activePreset.value}
            </div>
          )}
        </div>
      )}

      {mode === "custom" && (
        <div>
          <input
            type="text"
            value={value}
            onChange={(e) => onChange(e.target.value)}
            placeholder="Shell command (exit 0 = true)"
            className="mono"
            style={{ fontSize: 12, width: "100%" }}
          />
          <div
            style={{
              fontSize: 11,
              color: "var(--text-muted)",
              marginTop: 4,
            }}
          >
            Runs as a shell command. Exit code 0 = condition true. Requires approval.
          </div>
        </div>
      )}
    </div>
  );
}

// ── Template Snippet Chips ───────────────────────────────────────────

const TEMPLATE_SNIPPETS: { label: string; value: string; tooltip: string; category: string }[] = [
  // Variables
  { label: "workspace", value: "{{ workspace }}", tooltip: "Current workspace/branch name", category: "Variables" },
  { label: "repo", value: "{{ repo }}", tooltip: "Repository name", category: "Variables" },
  { label: "worktree_path", value: "{{ worktree_path }}", tooltip: "Absolute path to the worktree directory", category: "Variables" },
  { label: "short_commit", value: "{{ short_commit }}", tooltip: "Short (7-char) commit hash", category: "Variables" },
  { label: "service URL", value: "{{ service['SERVICE_NAME'].url }}", tooltip: "Full connection URL for a service (replace SERVICE_NAME)", category: "Variables" },
  { label: "commit", value: "{{ commit }}", tooltip: "Full commit hash", category: "Variables" },
  { label: "name", value: "{{ name }}", tooltip: "Project name", category: "Variables" },
  { label: "default_workspace", value: "{{ default_workspace }}", tooltip: "Default workspace name (e.g. main)", category: "Variables" },
  // Filters
  { label: "| sanitize", value: "{{ workspace | sanitize }}", tooltip: "Sanitize for use as identifier (replace special chars with -)", category: "Filters" },
  { label: "| sanitize_db", value: "{{ workspace | sanitize_db }}", tooltip: "Sanitize for database name (replace special chars with _)", category: "Filters" },
  { label: "| hash_port", value: "{{ workspace | hash_port }}", tooltip: "Deterministic port number from workspace name", category: "Filters" },
];

function SnippetChip({
  snippet,
  onInsert,
}: {
  snippet: (typeof TEMPLATE_SNIPPETS)[number];
  onInsert: (value: string) => void;
}) {
  const [showTooltip, setShowTooltip] = useState(false);
  const chipRef = useRef<HTMLButtonElement>(null);

  return (
    <span style={{ position: "relative", display: "inline-block" }}>
      <button
        ref={chipRef}
        onClick={() => onInsert(snippet.value)}
        onMouseEnter={() => setShowTooltip(true)}
        onMouseLeave={() => setShowTooltip(false)}
        className="mono"
        style={{
          fontSize: 10,
          padding: "2px 7px",
          background: "var(--bg-secondary)",
          border: "1px solid var(--border)",
          borderRadius: 10,
          cursor: "pointer",
          color: "var(--accent)",
          transition: "background 0.1s, border-color 0.1s",
          lineHeight: "16px",
        }}
        onMouseDown={(e) => e.preventDefault()} // keep focus in field
      >
        {snippet.label}
      </button>
      {showTooltip && (
        <div
          style={{
            position: "absolute",
            bottom: "calc(100% + 6px)",
            left: "50%",
            transform: "translateX(-50%)",
            background: "var(--bg-primary)",
            border: "1px solid var(--border)",
            borderRadius: 6,
            padding: "6px 10px",
            fontSize: 11,
            color: "var(--text-primary)",
            whiteSpace: "nowrap",
            zIndex: 200,
            boxShadow: "0 2px 8px rgba(0,0,0,0.2)",
            pointerEvents: "none",
          }}
        >
          <div style={{ fontWeight: 600, marginBottom: 2 }}>{snippet.tooltip}</div>
          <div className="mono" style={{ color: "var(--text-muted)", fontSize: 10 }}>
            {snippet.value}
          </div>
        </div>
      )}
    </span>
  );
}

function TemplateFieldWrapper({
  template,
  onInsert,
  children,
}: {
  template?: boolean;
  onInsert: (snippet: string) => void;
  children: React.ReactNode;
}) {
  if (!template) return <>{children}</>;

  const varSnippets = TEMPLATE_SNIPPETS.filter((s) => s.category === "Variables");
  const filterSnippets = TEMPLATE_SNIPPETS.filter((s) => s.category === "Filters");

  return (
    <div>
      {children}
      <div
        style={{
          marginTop: 4,
          display: "flex",
          flexWrap: "wrap",
          gap: 3,
          alignItems: "center",
        }}
      >
        {varSnippets.map((s) => (
          <SnippetChip key={s.value} snippet={s} onInsert={onInsert} />
        ))}
        <span style={{ width: 1, height: 14, background: "var(--border)", margin: "0 2px" }} />
        {filterSnippets.map((s) => (
          <SnippetChip key={s.value} snippet={s} onInsert={onInsert} />
        ))}
      </div>
    </div>
  );
}

function KeyValueEditor({
  value,
  onChange,
}: {
  value: Record<string, string>;
  onChange: (v: Record<string, string>) => void;
}) {
  const entries = Object.entries(value);

  const update = (oldKey: string, newKey: string, newVal: string) => {
    const next = { ...value };
    if (oldKey !== newKey) {
      delete next[oldKey];
    }
    next[newKey] = newVal;
    onChange(next);
  };

  const remove = (key: string) => {
    const next = { ...value };
    delete next[key];
    onChange(next);
  };

  const add = () => {
    onChange({ ...value, "": "" });
  };

  return (
    <div>
      {entries.map(([k, v], i) => (
        <div key={i} style={{ display: "flex", gap: 4, marginBottom: 4 }}>
          <input
            type="text"
            value={k}
            onChange={(e) => update(k, e.target.value, v)}
            placeholder="KEY"
            className="mono"
            style={{ flex: 1, fontSize: 11 }}
          />
          <input
            type="text"
            value={v}
            onChange={(e) => update(k, k, e.target.value)}
            placeholder="value (template)"
            className="mono"
            style={{ flex: 2, fontSize: 11 }}
          />
          <button
            className="btn"
            onClick={() => remove(k)}
            style={{ padding: "2px 6px", fontSize: 11 }}
          >
            x
          </button>
        </div>
      ))}
      <button
        className="btn"
        onClick={add}
        style={{ fontSize: 11, padding: "2px 8px" }}
      >
        + Add Row
      </button>
    </div>
  );
}
