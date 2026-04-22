// Shape mirrors api/server/feed.go::Tick exactly.
// Any change there MUST bump this and bump the dashboard contract.

export interface Level {
  price_ticks: number;
  qty: number;
  order_count: number;
}

export interface StageLat {
  p50_ns: number;
  p99_ns: number;
}

export interface StageLatencies {
  parse: StageLat;
  apply: StageLat;
  analytics: StageLat;
  risk: StageLat;
  wire: StageLat;
}

export interface TradePrint {
  ts_ns: number;
  price_ticks: number;
  qty: number;
  aggressor: number; // 1 buy, -1 sell, 0 inside
}

export interface Tick {
  ts_ns: number;
  seq: number;
  mid_ticks: number;
  spread_ticks: number;
  microprice_ticks: number;
  spread_bps: number;
  bid_depth: number;
  ask_depth: number;
  imbalance: number;
  vpin: number;
  trade_velocity: number;
  regime: 0 | 1 | 2 | 3;
  lat_p50_ns: number;
  lat_p99_ns: number;
  lat_p999_ns?: number;
  lat_max_ns?: number;
  lat_jitter_ns?: number;
  dropped_total?: number;
  events_per_sec: number;
  breaker_halted: boolean;
  breaker_reason: string;
  gaps_last_minute: number;
  bids?: Level[];
  asks?: Level[];
  stages?: StageLatencies;
  symbol?: string;
  instrument_id?: number;
  trades?: TradePrint[];
}

export const REGIME_NAME = ['CALM', 'VOLATILE', 'AGGRESSIVE', 'CRITICAL'] as const;
