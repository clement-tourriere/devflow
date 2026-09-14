import { Link, useParams } from "react-router-dom";
import YamlEditor from "./YamlEditor";

export default function ConfigEditor() {
  const { "*": path } = useParams();
  const projectPath = path ? decodeURIComponent(path) : "";
  return (
    <div>
      <Link to={`/projects/${encodeURIComponent(projectPath)}`}>&larr; Back to project</Link>
      <h1 className="page-title">Configuration</h1>
      <YamlEditor projectPath={projectPath} />
    </div>
  );
}
