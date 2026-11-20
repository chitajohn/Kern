import { SPEEDS, type Speed } from "../catalog";

export function formatMinutes(total: number) {
  const hours = Math.floor(total / 60);
  const minutes = Math.round(total % 60);
  return hours > 0 ? `${hours}h ${minutes}m` : `${minutes}m`;
}

/** A day's worth of build hours is the bar's full scale. */
const SCALE_MINUTES = 300;

/**
 * Remaining build hours, with the burn rate that decides how long an hour
 * lasts: slow is a full 60 minutes, medium 30, max 15.
 */
export function HoursMeter({
  minutes,
  speed = "slow",
  compact = false,
}: {
  minutes: number;
  speed?: Speed;
  compact?: boolean;
}) {
  const rate = SPEEDS.find((item) => item.id === speed) ?? SPEEDS[0];
  const filled = Math.max(0, Math.min(1, minutes / SCALE_MINUTES));
  return (
    <div className={compact ? "hours" : "hours big"}>
      <div className="hours-figure">
        <span>{formatMinutes(minutes)}</span>
        <small>left</small>
      </div>
      <div className="meter" role="img" aria-label={`${formatMinutes(minutes)} of build hours left`}>
        <span style={{ width: `${filled * 100}%` }} />
      </div>
      {!compact && (
        <div className="hours-foot">
          {rate.label.toLowerCase()} · 1 hour = {rate.minutes}m
        </div>
      )}
    </div>
  );
}
