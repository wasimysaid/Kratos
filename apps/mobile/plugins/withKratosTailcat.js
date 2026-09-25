const { withAppBuildGradle } = require('expo/config-plugins');

const dependency =
  "implementation files(rootProject.file('../../../target/tailcat/KratosTailcat.aar'))";

module.exports = function withKratosTailcat(config) {
  return withAppBuildGradle(config, (androidConfig) => {
    if (!androidConfig.modResults.contents.includes(dependency)) {
      androidConfig.modResults.contents = androidConfig.modResults.contents.replace(
        'dependencies {',
        `dependencies {\n    ${dependency}`,
      );
    }
    return androidConfig;
  });
};
