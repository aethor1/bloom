import { useEffect, useState } from "react";
import { motion } from "framer-motion";
import "./AgentIsland.css";

/** As sent by the backend's `agent-event` (see agents.rs). */
export interface AgentEvent {
	agent: AgentName;
	kind: "working" | "done" | "question" | "interrupted";
	session: string;
	title: string;
	project: string;
	message: string;
	asks: boolean;
	startedAt: number;
	durationMs: number | null;
	outputTokens: number;
	contextTokens: number;
	at: number;
}

export type AgentName = "claude" | "codex";
export type AgentStates = Partial<Record<AgentName, AgentEvent>>;

/** Claude always takes the left side of the island, Codex the right. */
export const AGENT_SIDES: readonly AgentName[] = ["claude", "codex"];
const LABEL: Record<AgentName, string> = { claude: "Claude", codex: "Codex" };

export function formatDuration(ms: number | null | undefined): string {
	if (ms == null || ms < 0) return "—";
	const s = Math.round(ms / 1000);
	if (s < 60) return `${s}s`;
	const m = Math.floor(s / 60);
	if (m < 60) return `${m}m ${String(s % 60).padStart(2, "0")}s`;
	return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, "0")}m`;
}

export function formatTokens(n: number): string {
	if (n < 1000) return `${n}`;
	if (n < 100_000) return `${(n / 1000).toFixed(1).replace(/\.0$/, "")}k`;
	if (n < 1_000_000) return `${Math.round(n / 1000)}k`;
	return `${(n / 1_000_000).toFixed(2).replace(/0$/, "")}M`;
}

function ago(at: number, now: number): string {
	const s = Math.max(0, Math.round((now - at) / 1000));
	if (s < 10) return "just now";
	if (s < 60) return `${s}s ago`;
	const m = Math.floor(s / 60);
	if (m < 60) return `${m}m ago`;
	const h = Math.floor(m / 60);
	return h < 24 ? `${h}h ago` : `${Math.floor(h / 24)}d ago`;
}

type Status = "done" | "asks" | "question" | "working" | "interrupted" | "idle";

function statusOf(ev: AgentEvent | undefined): Status {
	if (!ev) return "idle";
	if (ev.kind === "done") return ev.asks ? "asks" : "done";
	return ev.kind;
}

const STATUS_TEXT: Record<Status, string> = {
	done: "Done",
	asks: "Asked you",
	question: "Needs you",
	working: "Working",
	interrupted: "Stopped",
	idle: "Idle"
};

/** Re-renders every second while `active`, for live timers. */
function useNow(active: boolean): number {
	const [now, setNow] = useState(() => Date.now());
	useEffect(() => {
		if (!active) return;
		setNow(Date.now());
		const id = window.setInterval(() => setNow(Date.now()), 1000);
		return () => window.clearInterval(id);
	}, [active]);
	return now;
}

function turnDuration(ev: AgentEvent, now: number): number | null {
	return ev.kind === "working" ? now - ev.startedAt : ev.durationMs;
}

export function AgentMark({ agent, size = 18 }: { agent: AgentName; size?: number }) {
	if (agent === "claude") {
		// Eight tapered rays around a soft core.
		return (
			<svg className="agent-mark claude" width={size} height={size} viewBox="0 0 24 24" aria-hidden>
				{Array.from({ length: 8 }, (_, i) => (
					<path
						key={i}
						d="M12 1.6 13.15 9.4 12 11.2 10.85 9.4Z"
						transform={`rotate(${i * 45 + (i % 2 ? 6 : 0)} 12 12)`}
						fill="currentColor"
					/>
				))}
				<circle cx="12" cy="12" r="2.1" fill="currentColor" />
			</svg>
		);
	}
	// A terminal prompt in a rounded tile.
	return (
		<svg className="agent-mark codex" width={size} height={size} viewBox="0 0 24 24" aria-hidden>
			<rect x="2" y="3" width="20" height="18" rx="5.5" fill="none" stroke="currentColor" strokeWidth="1.9" />
			<path d="m7.2 9.2 3 2.8-3 2.8" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
			<path d="M12.6 15h4.4" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
		</svg>
	);
}

/** The agent's mark with a status ring: spinning while working, a soft
 *  pulse when something new is waiting. */
function MarkWithRing({ agent, status, fresh, size }: { agent: AgentName; status: Status; fresh: boolean; size: number }) {
	return (
		<span className={`agent-mark-ring status-${status} ${fresh ? "fresh" : ""}`} style={{ width: size + 12, height: size + 12 }}>
			{status === "working" && <span className="agent-spinner" />}
			<AgentMark agent={agent} size={size} />
		</span>
	);
}

/** Peek view: only what matters at a glance — who, what state, how long. */
export function AgentAlert({
	states,
	fresh,
	onOpen
}: {
	states: AgentStates;
	fresh: ReadonlySet<AgentName>;
	onOpen: () => void;
}) {
	const working = AGENT_SIDES.some((a) => states[a]?.kind === "working");
	const now = useNow(working);
	return (
		<motion.div
			className="agent-alert"
			onClick={(e) => {
				e.stopPropagation();
				onOpen();
			}}
			initial={{ opacity: 0 }}
			animate={{ opacity: 1 }}
			exit={{ opacity: 0, filter: "blur(4px)", transition: { duration: 0.12 } }}
			transition={{ duration: 0.2 }}
		>
			{AGENT_SIDES.map((agent) => {
				const ev = states[agent];
				const isFresh = fresh.has(agent);
				const status = statusOf(ev);
				// A side with nothing new stays a quiet mark, keeping its place.
				const loud = isFresh || status === "working";
				return (
					<motion.div
						key={agent}
						className={`agent-side side-${agent} ${loud ? "loud" : "quiet"}`}
						initial={{ opacity: 0, x: agent === "claude" ? -8 : 8 }}
						animate={{ opacity: 1, x: 0 }}
						transition={{ type: "spring", stiffness: 420, damping: 32, delay: 0.05 }}
					>
						<MarkWithRing agent={agent} status={loud ? status : "idle"} fresh={isFresh} size={18} />
						{loud && ev && (
							<div className="agent-side-text">
								<span className={`agent-side-status status-${status}`}>{STATUS_TEXT[status]}</span>
								<span className="agent-side-meta">
									{formatDuration(turnDuration(ev, now))}
									{ev.project ? ` · ${ev.project}` : ""}
								</span>
							</div>
						)}
					</motion.div>
				);
			})}
		</motion.div>
	);
}

function Stat({ value, label }: { value: string; label: string }) {
	return (
		<div className="agent-stat">
			<span className="agent-stat-value">{value}</span>
			<span className="agent-stat-label">{label}</span>
		</div>
	);
}

function AgentCard({ agent, ev, now }: { agent: AgentName; ev: AgentEvent | undefined; now: number }) {
	const status = statusOf(ev);
	return (
		<div className={`agent-card side-${agent} status-${status}`}>
			<div className="agent-card-head">
				<MarkWithRing agent={agent} status={status} fresh={false} size={15} />
				<span className="agent-card-name">{LABEL[agent]}</span>
				<span className={`agent-chip status-${status}`}>{STATUS_TEXT[status]}</span>
				{ev && <span className="agent-card-ago">{ago(ev.at, now)}</span>}
			</div>
			{ev ? (
				<>
					<div className="agent-card-title" title={ev.title || ev.project}>
						{ev.title || ev.project || "Untitled session"}
						{ev.title && ev.project && <span className="agent-card-project"> · {ev.project}</span>}
					</div>
					<p className={`agent-card-message ${status === "question" || status === "asks" ? "is-question" : ""}`}>
						{ev.kind === "working"
							? "Thinking…"
							: ev.kind === "interrupted"
								? "You stopped this turn."
								: ev.message || "No reply text."}
					</p>
					<div className="agent-stats">
						<Stat value={formatDuration(turnDuration(ev, now))} label={ev.kind === "working" ? "so far" : "thought"} />
						<Stat value={formatTokens(ev.outputTokens)} label="tokens out" />
						<Stat value={formatTokens(ev.contextTokens)} label="context" />
					</div>
				</>
			) : (
				<p className="agent-card-empty">No turns in the last few hours.</p>
			)}
		</div>
	);
}

/** Expanded view: both agents side by side, with the details. */
export function AgentsPanel({ states }: { states: AgentStates }) {
	const now = useNow(true);
	return (
		<motion.div
			className="agents-panel"
			onClick={(e) => e.stopPropagation()}
			initial={{ opacity: 0 }}
			animate={{ opacity: 1 }}
			exit={{ opacity: 0, filter: "blur(4px)", transition: { duration: 0.1 } }}
			transition={{ type: "spring", stiffness: 400, damping: 30 }}
		>
			{AGENT_SIDES.map((agent) => (
				<AgentCard key={agent} agent={agent} ev={states[agent]} now={now} />
			))}
		</motion.div>
	);
}
