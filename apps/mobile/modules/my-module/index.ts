// Re-export the native module. On web, it will be resolved to KratosTailcatModule.web.ts
// and on native platforms to KratosTailcatModule.ts
export { default } from './src/KratosTailcatModule';
export * from './src/KratosTailcat.types';
