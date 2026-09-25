import { NativeModule, requireOptionalNativeModule } from 'expo';

declare class KratosTailcatModule extends NativeModule<{}> {
  startProbe(invitation: string): Promise<string>;
  stop(): void;
}

export default requireOptionalNativeModule<KratosTailcatModule>('KratosTailcat');
