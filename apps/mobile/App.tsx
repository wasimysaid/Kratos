import { StatusBar } from 'expo-status-bar';
import { useState } from 'react';
import { Pressable, SafeAreaView, StyleSheet, Text, TextInput, View } from 'react-native';
import Tailcat from './modules/my-module';

export default function App() {
  const [invitation, setInvitation] = useState('');
  const [status, setStatus] = useState('Paste a one-use invitation from `kratos peer invite`.');
  const [url, setUrl] = useState('');
  const [busy, setBusy] = useState(false);

  const probe = async () => {
    if (!Tailcat) {
      setStatus('Install the Android development build first; Expo Go cannot load Tailcat.');
      return;
    }
    setBusy(true);
    setStatus('Starting Tailcat…');
    try {
      const relay = await fetch('https://tailcat.dev/derpmap.json');
      if (!relay.ok) throw new Error(`Tailcat relay map returned ${relay.status}.`);
      const nextUrl = await Tailcat.startProbe(invitation);
      setUrl(nextUrl);
      setStatus('Tailcat connected. The loopback proxy is live.');
    } catch (error) {
      setUrl('');
      setStatus(error instanceof Error ? error.message : 'Tailcat could not connect after reaching its relay map.');
    } finally {
      setBusy(false);
    }
  };

  const stop = () => {
    Tailcat?.stop();
    setUrl('');
    setStatus('Tailcat stopped.');
  };

  return (
    <SafeAreaView style={styles.screen}>
      <StatusBar style="light" />
      <View style={styles.card}>
        <Text style={styles.eyebrow}>ANDROID NATIVE GATE</Text>
        <Text style={styles.title}>Tailcat probe</Text>
        <Text style={styles.copy}>This proves the mobile app can start a secure Tailcat tunnel before we build the real client.</Text>
        <TextInput
          accessibilityLabel="Kratos pairing invitation"
          autoCapitalize="none"
          autoCorrect={false}
          multiline
          onChangeText={setInvitation}
          placeholder="kratos-pair:…"
          placeholderTextColor="#777770"
          style={styles.input}
          value={invitation}
        />
        <Pressable accessibilityRole="button" disabled={busy || !invitation.trim()} onPress={probe} style={[styles.button, (busy || !invitation.trim()) && styles.disabled]}>
          <Text style={styles.buttonText}>{busy ? 'Connecting…' : 'Start Tailcat probe'}</Text>
        </Pressable>
        {!!url && <Pressable accessibilityRole="button" onPress={stop} style={styles.stop}><Text style={styles.stopText}>Stop</Text></Pressable>}
        <Text accessibilityLiveRegion="polite" style={styles.status}>{status}</Text>
        {!!url && <Text selectable style={styles.url}>{url}</Text>}
      </View>
    </SafeAreaView>
  );
}

const styles = StyleSheet.create({
  screen: { alignItems: 'center', backgroundColor: '#111110', flex: 1, justifyContent: 'center', padding: 20 },
  card: { backgroundColor: '#191918', borderColor: '#343431', borderRadius: 16, borderWidth: 1, maxWidth: 520, padding: 20, width: '100%' },
  eyebrow: { color: '#a7a79f', fontSize: 11, fontWeight: '700', letterSpacing: 1.1 },
  title: { color: '#f1f1ed', fontSize: 27, fontWeight: '700', marginTop: 7 },
  copy: { color: '#afaea8', fontSize: 15, lineHeight: 22, marginTop: 8 },
  input: { backgroundColor: '#121211', borderColor: '#393936', borderRadius: 10, borderWidth: 1, color: '#f1f1ed', fontSize: 14, lineHeight: 20, marginTop: 20, minHeight: 130, padding: 12, textAlignVertical: 'top' },
  button: { alignItems: 'center', backgroundColor: '#d8ddd1', borderRadius: 10, marginTop: 12, padding: 13 },
  disabled: { backgroundColor: '#474742' },
  buttonText: { color: '#171715', fontSize: 15, fontWeight: '700' },
  stop: { alignItems: 'center', borderColor: '#555550', borderRadius: 10, borderWidth: 1, marginTop: 10, padding: 11 },
  stopText: { color: '#deddd8', fontSize: 15, fontWeight: '700' },
  status: { color: '#bebdb6', fontSize: 14, lineHeight: 20, marginTop: 18 },
  url: { color: '#c5dcb9', fontFamily: 'monospace', fontSize: 13, marginTop: 8 },
});
