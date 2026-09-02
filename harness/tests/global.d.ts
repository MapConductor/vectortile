export {};

declare global {
    interface Window {
        __harness: {
            ready: Promise<{ template: string; renderMs: number }>;
            template?: string;
            benchmark?: (
                z?: number,
                count?: number,
            ) => Promise<{ count: number; medianMs: number; minMs: number; maxMs: number }>;
        };
    }
}
