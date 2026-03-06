interface FormFieldProps {
  label: string;
  description?: string;
  required?: boolean;
  error?: string;
  children: React.ReactNode;
}

function FormField({ label, description, required, error, children }: FormFieldProps) {
  return (
    <div style={{ marginBottom: 16 }}>
      <label
        style={{
          display: "block",
          fontSize: 12,
          fontWeight: 600,
          color: "var(--text-primary)",
          marginBottom: 4,
        }}
      >
        {label}
        {required && <span style={{ color: "var(--danger)", marginLeft: 2 }}>*</span>}
      </label>
      {description && (
        <div
          style={{
            fontSize: 11,
            color: "var(--text-muted)",
            marginBottom: 6,
          }}
        >
          {description}
        </div>
      )}
      {children}
      {error && (
        <div
          style={{
            fontSize: 11,
            color: "var(--danger)",
            marginTop: 4,
          }}
        >
          {error}
        </div>
      )}
    </div>
  );
}

export default FormField;
