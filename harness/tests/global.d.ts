export {};

declare global {
    interface Window {
        __harness: {
            ready: Promise<{ template: string; renderMs: number }>;
            template?: string;
        };
    }
}
