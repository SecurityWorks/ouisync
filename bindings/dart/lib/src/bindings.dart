import 'dart:ffi';
import 'dart:io';

import 'package:flutter/foundation.dart';
import 'package:path/path.dart';

/// Callback for `start_service` and `stop_service`.
typedef StatusCallback = Void Function(Pointer<Void>, Uint16);

/// Callback for `init_log`.
typedef LogCallback = Void Function(Uint8, Pointer<Uint8>, Uint64, Uint64);

///
typedef StartService = Pointer<Void> Function(
  Pointer<Char>,
  Pointer<Char>,
  Pointer<NativeFunction<StatusCallback>>,
  Pointer<Void>,
);

typedef _StartServiceC = Pointer<Void> Function(
  Pointer<Char>,
  Pointer<Char>,
  Pointer<NativeFunction<StatusCallback>>,
  Pointer<Void>,
);

typedef StopService = void Function(
    Pointer<Void>, Pointer<NativeFunction<StatusCallback>>, Pointer<Void>);

typedef _StopServiceC = Void Function(
    Pointer<Void>, Pointer<NativeFunction<StatusCallback>>, Pointer<Void>);

typedef InitLog = int Function(
  Pointer<Char>,
  Pointer<NativeFunction<LogCallback>>,
);

typedef _InitLogC = Uint16 Function(
  Pointer<Char>,
  Pointer<NativeFunction<LogCallback>>,
);

typedef ReleaseLogMessage = void Function(Pointer<Uint8>, int, int);
typedef _ReleaseLogMessageC = Void Function(Pointer<Uint8>, Uint64, Uint64);

class Bindings {
  Bindings(DynamicLibrary library)
      : startService = library
            .lookupFunction<_StartServiceC, StartService>('start_service'),
        stopService =
            library.lookupFunction<_StopServiceC, StopService>('stop_service'),
        initLog = library.lookupFunction<_InitLogC, InitLog>('init_log'),
        releaseLogMessage =
            library.lookupFunction<_ReleaseLogMessageC, ReleaseLogMessage>(
                'release_log_message');

  /// Bidings instance that uses the default library.
  static Bindings instance = Bindings(_defaultLib());

  final StartService startService;
  final StopService stopService;
  final InitLog initLog;
  final ReleaseLogMessage releaseLogMessage;
}

DynamicLibrary _defaultLib() {
  final env = Platform.environment;

  // the default library name depends on the operating system
  late final String name;
  final base = 'ouisync_service';
  if (Platform.isLinux || Platform.isAndroid) {
    name = 'lib$base.so';
  } else if (Platform.isWindows) {
    name = '$base.dll';
  } else if (Platform.isIOS || Platform.isMacOS) {
    name = 'lib$base.dylib';
  } else {
    throw Exception('unsupported platform ${Platform.operatingSystem}');
  }

  // full path to loadable library
  final String path;

  if (env.containsKey('OUISYNC_LIB')) {
    // user provided library path
    path = env['OUISYNC_LIB']!;
  } else if (env.containsKey('FLUTTER_TEST')) {
    // guess the location of flutter's build output
    final String build;
    if (Platform.isMacOS) {
      build = join(dirname(Platform.script.toFilePath()), 'ouisync');
    } else {
      build = join('..', '..');
    }
    path = join(build, 'target', kReleaseMode ? 'release' : 'debug', name);
  } else {
    // assume that the library is available globally by name only
    path = name;
  }

  return DynamicLibrary.open(path);
}
