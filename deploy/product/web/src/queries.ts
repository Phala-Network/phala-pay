// The page's server state, read and changed through TanStack Query: every read is a query (polled
// where the page follows something live), every change a mutation that invalidates what it moves.

import { QueryClient, skipToken, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  cancelRefund,
  createDepositAddress,
  createQuote,
  createRefund,
  getAccount,
  getNetworks,
  getDepositAddress,
  getSweeps,
  getTimeline,
  getTrust,
  markRefundPaid,
  isTerminalApiError,
  type Selection,
} from "./api.js";

import {
  ACCOUNT_IDLE_INTERVAL_MS,
  ACCOUNT_PENDING_INTERVAL_MS,
  CONFIG_STALE_TIME_MS,
  EXPIRED_QUOTE_INTERVAL_MS,
  QUERY_RECOVERY_INTERVAL_MS,
  QUERY_RETRY_LIMIT,
  SWEEPS_INTERVAL_MS,
  TERMINAL_ACCOUNT_STATUSES,
  TERMINAL_DEPOSIT_STATUSES,
  TERMINAL_REFUND_STATUSES,
  TIMELINE_ACTIVE_INTERVAL_MS,
  TIMELINE_INTERVAL_MS,
} from "./polling.js";

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // Permanent client refusals are answers, not outages; timeouts and rate limits can recover.
      retry: (failures, error) => !isTerminalApiError(error) && failures < QUERY_RETRY_LIMIT,
    },
    mutations: { retry: false },
  },
});

export const keys = {
  account: ["account"] as const,
  networks: ["networks"] as const,
  trust: ["trust"] as const,
  depositAddress: ["deposit-address"] as const,
  sweeps: ["sweeps"] as const,
  timelines: ["timeline"] as const,
  timeline: (selection: Selection | null) => ["timeline", selection?.kind, selection?.id] as const,
};

/** The visitor's demo account: balance, ledger lines, and payments. */
export function useAccount() {
  return useQuery({
    queryKey: keys.account,
    queryFn: ({ signal }) => getAccount(signal),
    refetchInterval: (query) => isTerminalApiError(query.state.error) ? false :
      query.state.data?.payments.some((payment) =>
        !TERMINAL_ACCOUNT_STATUSES.has(payment.status),
      ) ? ACCOUNT_PENDING_INTERVAL_MS : ACCOUNT_IDLE_INTERVAL_MS,
  });
}

/** The networks, and their tokens, a customer can pay with; the service's config changes rarely. */
export function useNetworks() {
  return useQuery({
    queryKey: keys.networks,
    queryFn: ({ signal }) => getNetworks(signal),
    staleTime: CONFIG_STALE_TIME_MS,
    refetchInterval: (query) => query.state.status === "error" && !isTerminalApiError(query.state.error) ? QUERY_RECOVERY_INTERVAL_MS : false,
  });
}

/** The service's attestation, checked by the product (cached there for 5 minutes). */
export function useTrust() {
  return useQuery({
    queryKey: keys.trust,
    queryFn: ({ signal }) => getTrust(signal),
    staleTime: CONFIG_STALE_TIME_MS,
    refetchInterval: (query) => query.state.status === "error" && !isTerminalApiError(query.state.error) ? QUERY_RECOVERY_INTERVAL_MS : false,
  });
}

/** The followed payment's timeline, live. */
export function useTimeline(selection: Selection | null) {
  return useQuery({
    queryKey: keys.timeline(selection),
    queryFn: selection === null ? skipToken : ({ signal }) => getTimeline(selection, signal),
    refetchInterval: (query) => {
      if (isTerminalApiError(query.state.error)) {
        return false;
      }
      const timeline = query.state.data;
      // Marked-paid refunds stay pending until their transfer is verified at finality.
      if (timeline?.refunds.some((refund) => !TERMINAL_REFUND_STATUSES.has(refund.status))) {
        return TIMELINE_ACTIVE_INTERVAL_MS;
      }
      if (timeline?.deposit?.status === "reversed") {
        return false;
      }
      if (timeline?.deposit?.status === "credited" && timeline.deposit.swept) {
        return timeline.steps.some((step) => step.key === "webhook_received" && step.state === "current")
          ? TIMELINE_INTERVAL_MS : false;
      }
      if (timeline?.deposit === null && timeline.sent === null && timeline.quote !== null) {
        if (timeline.quote.status === "canceled") {
          return false;
        }
        if (timeline.quote.status === "expired" ||
            (timeline.quote.status === "open" && Date.now() >= timeline.quote.expires_at * 1000)) {
          return EXPIRED_QUOTE_INTERVAL_MS;
        }
      }
      const inFlight = timeline?.deposit != null
        ? !TERMINAL_DEPOSIT_STATUSES.has(timeline.deposit.status)
        : timeline?.sent != null;
      return inFlight ? TIMELINE_ACTIVE_INTERVAL_MS : TIMELINE_INTERVAL_MS;
    },
  });
}

/** The visitor's deposit address and its payments, followed once the product has shown it. */
export function useDepositAddress(enabled: boolean) {
  return useQuery({
    queryKey: keys.depositAddress,
    queryFn: ({ signal }) => getDepositAddress(signal),
    enabled,
  });
}

export function useSweeps() {
  return useQuery({ queryKey: keys.sweeps, queryFn: ({ signal }) => getSweeps(signal), refetchInterval: SWEEPS_INTERVAL_MS });
}

export function useCreateQuote() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: createQuote,
    onSuccess: () => client.invalidateQueries({ queryKey: keys.account }),
  });
}

export function useCreateDepositAddress() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: createDepositAddress,
    onSuccess: (created) => {
      client.setQueryData(keys.depositAddress, created);
      return client.invalidateQueries({ queryKey: keys.account });
    },
  });
}

/** A refund's change moves its timeline, and the balance once the service settles it. */
function useRefundMutation<T>(mutationFn: (variables: T) => Promise<unknown>) {
  const client = useQueryClient();
  return useMutation({
    mutationFn,
    onSuccess: () =>
      Promise.all([
        client.invalidateQueries({ queryKey: keys.timelines }),
        client.invalidateQueries({ queryKey: keys.account }),
      ]),
  });
}

export function useCreateRefund() {
  return useRefundMutation(createRefund);
}

export function useMarkRefundPaid() {
  return useRefundMutation(markRefundPaid);
}

export function useCancelRefund() {
  return useRefundMutation(cancelRefund);
}
