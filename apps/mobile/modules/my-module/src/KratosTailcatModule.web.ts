import { registerWebModule, NativeModule } from 'expo';

// KratosTailcatModule is not available on the web platform.
class KratosTailcatModule extends NativeModule<{}> {
  async startProbe(): Promise<string> {
    throw new Error('Tailcat requires an Android development build.');
  }

  stop(): void {}
}

export default registerWebModule(KratosTailcatModule, 'KratosTailcatModule');
