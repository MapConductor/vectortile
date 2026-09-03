export {};

declare global {
    interface Window {
        __harness: {
            ready: Promise<{ template: string; renderMs: number }>;
            template?: string;
            mode?: 'worker' | 'inline';
            mapIdle?: Promise<void>;
            setColor?: (layerId: string, property: string, value: string) => Promise<number>;
            resetColors?: () => Promise<void>;
            listColors?: () => { layerId: string; property: string; isExpression: boolean }[];
            benchmark?: (
                z?: number,
                count?: number,
            ) => Promise<{ count: number; medianMs: number; minMs: number; maxMs: number }>;
            stallTest?: (
                mode: 'worker' | 'inline',
                count?: number,
            ) => Promise<{ mode: string; longestStallMs: number; totalMs: number; tiles: number }>;
            fetchHookTest?: () => Promise<{ mode: string; warnings: string[]; bytes: number }>;
        };
    }
}
