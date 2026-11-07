import { useEffect, useLayoutEffect, useRef, useState } from "react";
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

/** A question an agent is waiting on, answerable from the notch (`agent-ask`). */
export interface AgentAsk {
	id: string;
	agent: AgentName;
	project: string;
	questions: {
		question: string;
		header: string;
		multiSelect: boolean;
		options: { label: string; description: string }[];
	}[];
	/** False for Codex: a pick is copied for pasting into Codex instead. */
	answerable: boolean;
}

export type AgentName = "claude" | "codex";
export type AgentStates = Partial<Record<AgentName, AgentEvent>>;
export type AgentAsks = Partial<Record<AgentName, AgentAsk>>;
/** Question text → chosen label(s), or null to answer in the agent's own app. */
export type AskAnswer = Record<string, string> | null;

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

function statusOf(ev: AgentEvent | undefined, ask: AgentAsk | undefined): Status {
	if (ask) return "question";
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

/** Reports the element's natural height so the notch can grow to fit it. */
function useReportHeight(onHeight: ((h: number) => void) | undefined) {
	const ref = useRef<HTMLDivElement>(null);
	useLayoutEffect(() => {
		const el = ref.current;
		if (!el || !onHeight) return;
		const report = () => onHeight(Math.ceil(el.getBoundingClientRect().height / (parseFloat(getComputedStyle(el).zoom) || 1)));
		report();
		const ro = new ResizeObserver(report);
		ro.observe(el);
		return () => ro.disconnect();
	}, [onHeight]);
	return ref;
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

/** One pending question at a time, its options as buttons. Multi-select
 *  questions toggle options and send with a button. */
function AskView({ ask, onAnswer }: { ask: AgentAsk; onAnswer: (ask: AgentAsk, answer: AskAnswer) => void }) {
	const [step, setStep] = useState(0);
	const [answers, setAnswers] = useState<Record<string, string>>({});
	const [picked, setPicked] = useState<string[]>([]);
	// A new question replaces whatever was half answered.
	useEffect(() => {
		setStep(0);
		setAnswers({});
		setPicked([]);
	}, [ask.id]);
	const q = ask.questions[Math.min(step, ask.questions.length - 1)];
	const commit = (value: string) => {
		const next = { ...answers, [q.question]: value };
		setPicked([]);
		if (step + 1 < ask.questions.length) {
			setAnswers(next);
			setStep(step + 1);
		} else {
			onAnswer(ask, next);
		}
	};
	return (
		<div className="agent-ask" onClick={(e) => e.stopPropagation()}>
			<div className="agent-ask-head">
				<span className="agent-ask-who">
					{LABEL[ask.agent]} needs you{ask.project ? <span className="agent-ask-project"> · {ask.project}</span> : null}
				</span>
				{ask.questions.length > 1 && (
					<span className="agent-ask-step">
						{step + 1}/{ask.questions.length}
					</span>
				)}
			</div>
			<motion.p key={`${ask.id}-${step}`} className="agent-ask-question" initial={{ opacity: 0, y: 3 }} animate={{ opacity: 1, y: 0 }}>
				{q.question}
			</motion.p>
			<div className="agent-ask-options">
				{q.options.map((o, i) => {
					const on = picked.includes(o.label);
					return (
						<motion.button
							key={`${step}-${o.label}`}
							type="button"
							className={`agent-ask-option ${on ? "on" : ""}`}
							title={o.description || undefined}
							initial={{ opacity: 0, scale: 0.94 }}
							animate={{ opacity: 1, scale: 1 }}
							transition={{ delay: 0.03 * i, type: "spring", stiffness: 520, damping: 30 }}
							whileTap={{ scale: 0.95 }}
							onClick={() => {
								if (!q.multiSelect) return commit(o.label);
								setPicked((p) => (on ? p.filter((x) => x !== o.label) : [...p, o.label]));
							}}
						>
							<span className="agent-ask-option-label">{o.label}</span>
							{o.description && <span className="agent-ask-option-desc">{o.description}</span>}
						</motion.button>
					);
				})}
			</div>
			<div className="agent-ask-foot">
				{!ask.answerable && <span className="agent-ask-hint">A pick is copied — paste it in Codex</span>}
				<button type="button" className="agent-ask-release" onClick={() => onAnswer(ask, null)}>
					{ask.answerable ? "Answer in app" : "Open Codex"}
				</button>
				{q.multiSelect && (
					<button
						type="button"
						className="agent-ask-send"
						disabled={picked.length === 0}
						onClick={() => commit(picked.join(", "))}
					>
						Send
					</button>
				)}
			</div>
		</div>
	);
}

/** Peek view: who, what state, how long — and, for a side waiting on you,
 *  the question with its answers. */
export function AgentAlert({
	states,
	asks,
	fresh,
	onOpen,
	onAnswer,
	onHeight
}: {
	states: AgentStates;
	asks: AgentAsks;
	fresh: ReadonlySet<AgentName>;
	onOpen: () => void;
	onAnswer: (ask: AgentAsk, answer: AskAnswer) => void;
	onHeight?: (h: number) => void;
}) {
	const working = AGENT_SIDES.some((a) => states[a]?.kind === "working");
	const now = useNow(working);
	const ref = useReportHeight(onHeight);
	const asking = AGENT_SIDES.filter((a) => asks[a]);
	return (
		<motion.div
			ref={ref}
			className={`agent-alert ${asking.length ? "has-ask" : ""}`}
			style={{
				gridTemplateColumns: asking.length
					? AGENT_SIDES.map((a) => (asks[a] ? "minmax(0, 1fr)" : "auto")).join(" ")
					: undefined
			}}
			onClick={(e) => {
				e.stopPropagation();
				if (!asking.length) onOpen();
			}}
			initial={{ opacity: 0 }}
			animate={{ opacity: 1 }}
			exit={{ opacity: 0, filter: "blur(4px)", transition: { duration: 0.12 } }}
			transition={{ duration: 0.2 }}
		>
			{AGENT_SIDES.map((agent) => {
				const ev = states[agent];
				const ask = asks[agent];
				const isFresh = fresh.has(agent) || !!ask;
				const status = statusOf(ev, ask);
				// A side with nothing new stays a quiet mark, keeping its place.
				const loud = isFresh || status === "working";
				return (
					<motion.div
						key={agent}
						layout
						className={`agent-side side-${agent} ${loud ? "loud" : "quiet"} ${ask ? "asking" : ""}`}
						initial={{ opacity: 0, x: agent === "claude" ? -8 : 8 }}
						animate={{ opacity: 1, x: 0 }}
						transition={{ type: "spring", stiffness: 420, damping: 32 }}
					>
						<MarkWithRing agent={agent} status={loud ? status : "idle"} fresh={isFresh} size={18} />
						{ask ? (
							<AskView ask={ask} onAnswer={onAnswer} />
						) : (
							loud &&
							ev &&
							// Next to a question, the other side stays compact.
							!asking.length && (
								<div className="agent-side-text">
									<span className={`agent-side-status status-${status}`}>{STATUS_TEXT[status]}</span>
									<span className="agent-side-meta">
										{formatDuration(turnDuration(ev, now))}
										{ev.project ? ` · ${ev.project}` : ""}
									</span>
								</div>
							)
						)}
					</motion.div>
				);
			})}
		</motion.div>
	);
}

function AgentCard({
	agent,
	ev,
	ask,
	now,
	onAnswer
}: {
	agent: AgentName;
	ev: AgentEvent | undefined;
	ask: AgentAsk | undefined;
	now: number;
	onAnswer: (ask: AgentAsk, answer: AskAnswer) => void;
}) {
	const status = statusOf(ev, ask);
	return (
		<div className={`agent-card side-${agent} status-${status}`}>
			<div className="agent-card-head">
				<MarkWithRing agent={agent} status={status} fresh={false} size={15} />
				<span className="agent-card-name">{LABEL[agent]}</span>
				<span className={`agent-chip status-${status}`}>{STATUS_TEXT[status]}</span>
				{ev && <span className="agent-card-ago">{ago(ev.at, now)}</span>}
			</div>
			{ask ? (
				<AskView ask={ask} onAnswer={onAnswer} />
			) : ev ? (
				<>
					<p className={`agent-card-message ${status === "question" || status === "asks" ? "is-question" : ""}`}>
						{ev.kind === "working"
							? "Thinking…"
							: ev.kind === "interrupted"
								? "You stopped this turn."
								: ev.message || "No reply text."}
					</p>
					<div className="agent-card-foot">
						<span>{ev.project}</span>
						<span>
							{ev.kind === "working" ? "thinking " : "thought "}
							<b>{formatDuration(turnDuration(ev, now))}</b>
						</span>
					</div>
				</>
			) : (
				<p className="agent-card-empty">No turns in the last few hours.</p>
			)}
		</div>
	);
}

/** Expanded view: both agents side by side. */
export function AgentsPanel({
	states,
	asks,
	onAnswer,
	onHeight
}: {
	states: AgentStates;
	asks: AgentAsks;
	onAnswer: (ask: AgentAsk, answer: AskAnswer) => void;
	onHeight?: (h: number) => void;
}) {
	const now = useNow(true);
	const ref = useReportHeight(onHeight);
	return (
		<motion.div
			ref={ref}
			className="agents-panel"
			onClick={(e) => e.stopPropagation()}
			initial={{ opacity: 0 }}
			animate={{ opacity: 1 }}
			exit={{ opacity: 0, filter: "blur(4px)", transition: { duration: 0.1 } }}
			transition={{ type: "spring", stiffness: 400, damping: 30 }}
		>
			{AGENT_SIDES.map((agent) => (
				<AgentCard key={agent} agent={agent} ev={states[agent]} ask={asks[agent]} now={now} onAnswer={onAnswer} />
			))}
		</motion.div>
	);
}
