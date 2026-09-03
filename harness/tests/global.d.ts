export {};

declare global {
    interface Window {
        __harness: {
            ready: Promise<{ template: string; renderMs: number }>;
            template?: string;
            mode?: 'worker' | 'inline';
            stallTest?: (
                mode: 'worker' | 'inline',
                count?: number,
            ) => Promise<{ mode: string; longestStallMs: number; totalMs: number; tiles: number }>;
            fetchHookTest?: () => Promise<{ mode: string; warnings: string[]; bytes: number }>;
            benchmark?: (
                z?: number,
                count?: number,
            ) => Promise<{ count: number; medianMs: number; minMs: number; maxMs: number }>;
        };
    }
}
