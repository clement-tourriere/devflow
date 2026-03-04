import { useState } from "react";

interface TagListProps {
  values: string[];
  onChange: (values: string[]) => void;
  placeholder?: string;
}

function TagList({ values, onChange, placeholder = "Add item..." }: TagListProps) {
  const [input, setInput] = useState("");

  const handleAdd = () => {
    const trimmed = input.trim();
    if (trimmed && !values.includes(trimmed)) {
      onChange([...values, trimmed]);
      setInput("");
    }
  };

  const handleRemove = (index: number) => {
    onChange(values.filter((_, i) => i !== index));
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter") {
      e.preventDefault();
      handleAdd();
    }
  };

  return (
    <div>
      <div
        style={{
          display: "flex",
          flexWrap: "wrap",
          gap: 6,
          marginBottom: values.length > 0 ? 8 : 0,
        }}
      >
        {values.map((value, i) => (
          <span
            key={i}
            style={{
              display: "inline-flex",
              alignItems: "center",
              gap: 4,
              background: "var(--bg-tertiary)",
              border: "1px solid var(--border)",
              borderRadius: 4,
              padding: "2px 8px",
              fontSize: 12,
              color: "var(--text-primary)",
            }}
          >
            {value}
            <button
              onClick={() => handleRemove(i)}
              style={{
                background: "none",
                border: "none",
                color: "var(--text-muted)",
                cursor: "pointer",
                padding: 0,
                fontSize: 14,
                lineHeight: 1,
              }}
            >
              ×
            </button>
          </span>
        ))}
      </div>
      <div style={{ display: "flex", gap: 6 }}>
        <input
          type="text"
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={handleKeyDown}
          placeholder={placeholder}
          style={{ flex: 1, fontSize: 13 }}
        />
        <button className="btn" onClick={handleAdd} disabled={!input.trim()}>
          Add
        </button>
      </div>
    </div>
  );
}

export default TagList;
