import { Match, Switch } from "solid-js";
import { createForm } from "@tanstack/solid-form";
import { useQueryClient } from "@tanstack/solid-query";

import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import {
  buildOptionalBoolFormField,
  buildOptionalNumberFormField,
  buildOptionalSecretFormField,
  buildOptionalTextFormField,
  unsetOrValidUrl,
} from "@/components/FormFields";

import { BackupsConfig, Config, ServerConfig } from "@proto/config";
import { createConfigQuery, setConfig } from "@/lib/api/config";

const labelWidth = "w-44";

export function BackupSettings(props: {
  markDirty: () => void;
  postSubmit: () => void;
}) {
  const config = createConfigQuery();

  return (
    <Switch>
      <Match when={config.isError}>Failed to fetch config</Match>

      <Match when={config.isLoading}>Loading</Match>

      <Match when={config.data?.config !== undefined}>
        <BackupSettingsForm config={config.data!.config!} {...props} />
      </Match>
    </Switch>
  );
}

function BackupSettingsForm(props: {
  config: Config;
  markDirty: () => void;
  postSubmit: () => void;
}) {
  const queryClient = useQueryClient();

  function backupsConfig(config: Config): BackupsConfig {
    const backups = config.server?.backups;
    // "deep-copy" & fallback
    return backups
      ? BackupsConfig.decode(BackupsConfig.encode(backups).finish())
      : BackupsConfig.fromJSON({});
  }

  const form = createForm(() => ({
    defaultValues: backupsConfig(props.config),
    onSubmit: async ({ value }: { value: BackupsConfig }) => {
      const newConfig = Config.fromPartial(props.config);
      newConfig.server = newConfig.server ?? ServerConfig.fromJSON({});
      newConfig.server.backups = value;

      await setConfig({
        client: queryClient,
        config: newConfig,
        throw: true,
      });

      props.postSubmit?.();
    },
  }));

  form.useStore((state) => {
    if (state.isDirty && !state.isSubmitted) {
      props.markDirty();
    }
  });

  return (
    <form
      method="dialog"
      onSubmit={(e: SubmitEvent) => {
        e.preventDefault();
        form.handleSubmit();
      }}
    >
      <div class="flex flex-col gap-4">
        <Card>
          <CardHeader>
            <h2>Remote Backups (S3 / Cloudflare R2)</h2>
          </CardHeader>

          <CardContent class="flex flex-col gap-4">
            <p class="text-sm">
              When an endpoint and bucket are configured, dirty databases are
              uploaded to <code>latest/&lt;name&gt;.db</code> whenever their
              connection is evicted from the connection cache, and a nightly job
              sweeps all databases, keeps dated{" "}
              <code>epochs/&lt;date&gt;/</code> copies and prunes expired ones.
              Changes apply on server <b>restart</b>; <code>TB_BACKUP_*</code>{" "}
              environment variables take precedence over these settings.
            </p>

            <div>
              <form.Field name="s3Endpoint" validators={unsetOrValidUrl()}>
                {buildOptionalTextFormField({
                  label: () => <div class={labelWidth}>Endpoint</div>,
                  placeholder: "https://<account-id>.r2.cloudflarestorage.com",
                  info: (
                    <p>
                      S3-compatible endpoint. Leaving endpoint or bucket unset
                      disables backups.
                    </p>
                  ),
                })}
              </form.Field>
            </div>

            <div>
              <form.Field name="s3BucketName">
                {buildOptionalTextFormField({
                  label: () => <div class={labelWidth}>Bucket</div>,
                })}
              </form.Field>
            </div>

            <div>
              <form.Field name="s3Region">
                {buildOptionalTextFormField({
                  label: () => <div class={labelWidth}>Region</div>,
                  placeholder: "auto",
                  info: <p>For Cloudflare R2 keep the default "auto".</p>,
                })}
              </form.Field>
            </div>

            <div>
              <form.Field name="s3AccessKeyId">
                {buildOptionalTextFormField({
                  label: () => <div class={labelWidth}>Access Key ID</div>,
                })}
              </form.Field>
            </div>

            <div>
              <form.Field name="s3SecretAccessKey">
                {buildOptionalSecretFormField({
                  label: () => <div class={labelWidth}>Secret Access Key</div>,
                })}
              </form.Field>
            </div>

            <div>
              <form.Field name="schedule">
                {buildOptionalTextFormField({
                  label: () => <div class={labelWidth}>Schedule (UTC)</div>,
                  placeholder: "0 0 22 * * * *",
                  info: (
                    <p>
                      7-field cron spec with seconds, interpreted in UTC. The
                      default "0 0 22 * * * *" runs daily at 22:00 UTC, i.e.
                      03:00 in UTC+5.
                    </p>
                  ),
                })}
              </form.Field>
            </div>

            <div>
              <form.Field name="concurrency">
                {buildOptionalNumberFormField({
                  integer: true,
                  label: () => <div class={labelWidth}>Concurrency</div>,
                  info: (
                    <p>
                      Maximum concurrent snapshot+upload pipelines. Default: 2.
                    </p>
                  ),
                })}
              </form.Field>
            </div>

            <div>
              <form.Field name="epochRetainDays">
                {buildOptionalNumberFormField({
                  integer: true,
                  label: () => (
                    <div class={labelWidth}>Epoch Retention (days)</div>
                  ),
                  info: (
                    <p>
                      Nightly <code>epochs/&lt;date&gt;/</code> copies older
                      than this are deleted. Default: 14.
                    </p>
                  ),
                })}
              </form.Field>
            </div>

            <div>
              <form.Field name="includeMain">
                {buildOptionalBoolFormField({
                  label: () => <div class={labelWidth}>Include main.db</div>,
                  info: (
                    <p>
                      Whether the main database is backed up as well; logs and
                      session databases never leave the machine. Default: on.
                    </p>
                  ),
                })}
              </form.Field>
            </div>
          </CardContent>
        </Card>

        <div class="flex justify-end gap-4">
          <form.Subscribe
            selector={(state) => ({
              canSubmit: state.canSubmit,
              isSubmitting: state.isSubmitting,
            })}
          >
            {(state) => {
              return (
                <Button
                  type="submit"
                  disabled={!state().canSubmit}
                  variant="default"
                >
                  {state().isSubmitting ? "..." : "Submit"}
                </Button>
              );
            }}
          </form.Subscribe>
        </div>
      </div>
    </form>
  );
}
