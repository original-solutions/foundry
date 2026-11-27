import { useContext, type CSSProperties } from "react";
import { faceOf, Faces } from "./face";

/**
 * A project's face: its two letters on a small tile of its own hue, the same wherever its
 * workspaces show, so a long list reads by project at a glance. It pictures what the words beside
 * it say, so a screen reader skips it; pointing at it names the project (or `title`). The size is
 * the caller's (`className`); the colours are index.css's `.project-face`, mirrored for light.
 */
export function ProjectFace({ project, title = project, className = "size-4 text-[8px]" }: { project: string; title?: string; className?: string }) {
  const face = faceOf(project, useContext(Faces));
  return (
    <span
      aria-hidden="true"
      title={title}
      style={{ "--hue": face.hue } as CSSProperties}
      className={`project-face grid shrink-0 place-items-center rounded-[4px] font-sans leading-none font-semibold tracking-tight ${className}`}
    >
      {face.letters}
    </span>
  );
}
