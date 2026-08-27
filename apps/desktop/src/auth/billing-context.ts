import { createContext, useContext } from "react";

import type { BillingInfo } from "@anlg/supabase";

export type BillingAccess = BillingInfo & {
  isReady: boolean;
  canStartTrial: { data: boolean; isPending: boolean };
  upgradeToPro: () => void;
  isUpgradingToPro: boolean;
};

export const BillingContext = createContext<BillingAccess | null>(null);

export function useBillingAccess() {
  const context = useContext(BillingContext);

  if (!context) {
    throw new Error("useBillingAccess must be used within BillingProvider");
  }

  // tbm patch branch: local-only features (Auto template editing, dictionary,
  // automations) are gated on Pro but run entirely on this machine. Force the
  // Pro flag for them, and isReady too — signed out, the billing claims query
  // never resolves, which left Save & enable permanently disabled. isPaid is
  // left alone so hosted STT/cloud routing never pretends to have an
  // entitlement.
  return { ...context, isPro: true, isReady: true };
}
